#![doc(
    html_root_url = "https://docs.rs/bazel-execlog-cmp/0.1.0", // remember to bump!
)]

use std::collections::{HashMap, HashSet};
use std::env::args_os;
use std::fmt::{self, Debug};
use std::fs::read_to_string;
use std::mem::forget;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use color_eyre::eyre::{self, eyre};
use color_eyre::owo_colors::OwoColorize;
use fuzzy_matcher::{clangd::ClangdMatcher, FuzzyMatcher};
use indicatif::{MultiProgress, ProgressBar, ProgressIterator, ProgressStyle};
use rayon::prelude::*;
use rustyline::{
    completion::{Candidate, Completer},
    highlight::Highlighter,
    hint::Hinter,
    validate::Validator,
    CompletionType, Config, Editor, Helper,
};
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_number_from_string;
use serde_json::de::from_slice;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct Sha256(#[serde(with = "hex_serde")] [u8; 32]);

impl Debug for Sha256 {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        for x in &self.0 {
            write!(fmt, "{:02x}", x)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct Digest<'i> {
    hash: Sha256,
    #[serde(
        rename = "sizeBytes",
        deserialize_with = "deserialize_number_from_string"
    )]
    size_bytes: usize,
    #[serde(rename = "hashFunctionName")]
    hash_function_name: &'i str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct Item<'i> {
    #[serde(borrow)]
    path: &'i Path,
    digest: Digest<'i>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct EnvVar<'i> {
    name: &'i str,
    value: &'i str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ActionContext<'i> {
    #[serde(rename = "environmentVariables", borrow)]
    environment_variables: Vec<EnvVar<'i>>,
    inputs: Vec<Item<'i>>,
    #[serde(rename = "listedOutputs", borrow)]
    listed_outputs: Vec<&'i str>,
    remotable: bool,
    cacheable: bool,
    #[serde(rename = "actualOutputs", borrow)]
    actual_outputs: Vec<Item<'i>>,
}

type Output<'i> = &'i str;
#[cfg(feature = "json-dump-command")]
type BuildAction<'i> = (ActionContext<'i>, serde_json::Value);
#[cfg(not(feature = "json-dump-command"))]
type BuildAction<'i> = (ActionContext<'i>,);

type Map<'l> = HashMap<Output<'l>, Arc<BuildAction<'l>>>;

/// Execution logs are tricky since they're composed of concatenated JSON objects.
///
/// As in:
/// ```json
/// { "foo": true, bar: 8, ... }{ "foo": false, bar: 12, ... }
/// ```
fn exec_log_to_hashmap<'l>(
    log: &'l [u8],
    pb: &ProgressBar,
) -> eyre::Result<(Map<'l>, HashSet<&'l str>)> {
    let mut prev = 0;
    let mut curr = 0;
    let mut map = HashMap::new();

    let mut outputs_with_multiple_actions = HashSet::new();

    let mut process_obj = |j| -> eyre::Result<()> {
        #[cfg(feature = "json-dump-command")]
        let val = from_slice(j)?;
        let ctx: ActionContext = from_slice(j)?;

        let action = Arc::new((
            ctx,
            #[cfg(feature = "json-dump-command")]
            val,
        ));
        for output in action.0.listed_outputs.iter() {
            if map.insert(*output, action.clone()).is_some() {
                outputs_with_multiple_actions.insert(*output);
            }
        }

        Ok(())
    };

    while curr + 1 < log.len() {
        if let b"}{" = &log[curr..][..2] {
            process_obj(&log[prev..=curr])?;

            prev = curr + 1;
        }

        curr += 1;
        if curr % 10_000 == 0 {
            pb.inc(10_000);
        }
    }
    process_obj(&log[prev..])?;

    pb.finish();

    Ok((map, outputs_with_multiple_actions))
}

struct ExecLogHelper<'l> {
    map: &'l [(&'l String, Map<'l>)],
    fuzzy_matcher: ClangdMatcher,
}

impl<'l> ExecLogHelper<'l> {
    fn new(map: &'l [(&'l String, Map<'l>)]) -> Self {
        Self {
            map,
            fuzzy_matcher: ClangdMatcher::default().smart_case().use_cache(true),
        }
    }
}

impl<'l> Helper for ExecLogHelper<'l> {}

impl<'l> Validator for ExecLogHelper<'l> {}

impl<'l> Highlighter for ExecLogHelper<'l> {}

impl<'l> Hinter for ExecLogHelper<'l> {
    type Hint = <() as Hinter>::Hint;
}

enum ExecLogCompletionCandidate<'l> {
    CommandCompletion(&'static str),
    OutputSuggestion(String, &'l str),
}

impl<'l> ExecLogCompletionCandidate<'l> {
    const COMMANDS: &'static [&'static str] = &[
        "quit",
        "help",
        "cmp",
        "transitive-cmp",
        "tcmp",
        "edges",
        #[cfg(feature = "json-dump-command")]
        "json",
        "view",
        "diff",
    ];
}

impl<'l> Candidate for ExecLogCompletionCandidate<'l> {
    fn display(&self) -> &str {
        match self {
            Self::CommandCompletion(c) => c,
            Self::OutputSuggestion(p, _) => p,
        }
    }

    fn replacement(&self) -> &str {
        match self {
            Self::CommandCompletion(c) => c,
            Self::OutputSuggestion(_, p) => p,
        }
    }
}

impl<'l> Completer for ExecLogHelper<'l> {
    type Candidate = ExecLogCompletionCandidate<'l>;

    fn complete(
        &self,
        line: &str,
        _pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        if !line.contains(' ') {
            let mut v = Vec::new();
            for c in Self::Candidate::COMMANDS {
                if c.starts_with(line) {
                    v.push(ExecLogCompletionCandidate::CommandCompletion(c));
                }
            }

            Ok((0, v))
        } else if Self::Candidate::COMMANDS.contains(&line.split(' ').next().unwrap())
            && !(line.starts_with("quit ") || line.starts_with('q') || line.starts_with("help"))
        {
            let path = line.split_once(" ").map(|(_, p)| p).unwrap_or("");
            let idx = line.find(' ').unwrap() + 1;

            let mut matches: Vec<_> = self.map[0]
                .1
                .keys()
                .filter_map(|k| {
                    self.fuzzy_matcher
                        .fuzzy_indices(*k, path)
                        .map(|res| (res, *k))
                })
                .take(50)
                .map(|((score, indices), k)| {
                    let mut s = String::new();
                    let mut curr_idx = 0;

                    for (idx, c) in k.char_indices() {
                        if indices.get(curr_idx).map(|i| *i == idx).unwrap_or(false) {
                            s.push_str(&format!("{}", c.blue()));
                            curr_idx += 1;
                        } else {
                            s.push(c);
                        }
                    }

                    (score, ExecLogCompletionCandidate::OutputSuggestion(s, k))
                })
                .collect();
            matches.sort_by_key(|(score, _)| *score);

            Ok((idx, matches.into_iter().map(|(_, o)| o).collect()))
        } else {
            Ok((0, vec![]))
        }
    }
}

type ArtifactName<'l> = &'l str;

fn find_mismatched<'l>(
    artifact: ArtifactName<'l>,
    actions: impl Iterator<Item = (&'l String, &'l Arc<BuildAction<'l>>)>,
) -> (
    impl Iterator<Item = (ArtifactName<'l>, &'l str)>, // env vars
    impl Iterator<Item = (ArtifactName<'l>, &'l Path)>, // inputs
    impl Iterator<Item = (ArtifactName<'l>, &'l Path)>, // outputs
) {
    let mut env_vars: HashMap<&str, (&str, usize)> = HashMap::new();
    let mut inputs: HashMap<&Path, (&Digest, usize)> = HashMap::new();
    let mut outputs: HashMap<&Path, (&Digest, usize)> = HashMap::new();

    // TODO: DRY
    let mut num_files = 0;
    for (_, a) in actions {
        for e in a.0.environment_variables.iter() {
            let (val, count) = env_vars.entry(e.name).or_insert((e.value, 0));
            if *val == e.value {
                *count += 1;
            }
        }

        // `HashSet` for dedupe; inputs get listed multiple times, sometimes
        for i in a.0.inputs.iter().collect::<HashSet<_>>().iter() {
            let (val, count) = inputs.entry(i.path).or_insert((&i.digest, 0));
            if *val == &i.digest {
                *count += 1;
            }
        }

        for o in a.0.actual_outputs.iter() {
            let (val, count) = outputs.entry(o.path).or_insert((&o.digest, 0));
            if *val == &o.digest {
                *count += 1;
            }
        }

        num_files += 1;
    }

    let mismatched_env_vars = env_vars
        .into_iter()
        .filter(move |(_, (_, c))| *c != num_files)
        .map(move |(k, _)| (artifact, k));
    let mismatched_inputs = inputs
        .into_iter()
        .filter(move |(_, (_, c))| *c != num_files)
        .map(move |(k, _)| (artifact, k));
    let mismatched_outputs = outputs
        .into_iter()
        .filter(move |(_, (_, c))| *c != num_files)
        .map(move |(k, _)| (artifact, k));

    (mismatched_env_vars, mismatched_inputs, mismatched_outputs)
}

fn print_mismatched<'l>(
    (env, inp, out): (
        impl Iterator<Item = (ArtifactName<'l>, &'l str)> + 'l, // env vars
        impl Iterator<Item = (ArtifactName<'l>, &'l Path)> + 'l, // inputs
        impl Iterator<Item = (ArtifactName<'l>, &'l Path)> + 'l, // outputs
    ),
    maps: &'l [(&'l String, Map<'l>)],
) {
    let mut mismatched = false;

    let mut mismatched_env_vars = env.peekable();
    if mismatched_env_vars.peek().is_some() {
        mismatched = true;
        println!("\n{}:", "Environment Variable Mismatches".bold());
    }
    for (artifact, env_name) in mismatched_env_vars {
        println!("  ${}", env_name.blue());
        for (f, m) in maps.iter() {
            print!("    {:>20.20}: ", f.dimmed());
            if let Some(v) = m[artifact]
                .0
                .environment_variables
                .iter()
                .find(|e| e.name == env_name)
            {
                println!("{}", v.value.yellow());
            } else {
                println!("{}", "<not present>".red());
            }
        }
    }

    fn item_mismatch_printer<'l>(
        it: impl Iterator<Item = (ArtifactName<'l>, &'l Path)>,
        name: &'static str,
        ctx_to_item_vec: impl Fn(&'l ActionContext<'l>) -> &'l Vec<Item<'l>>,
        maps: &'l [(&'l String, Map<'l>)],
        mismatched: &mut bool,
    ) {
        let mut it = it.peekable();
        if it.peek().is_some() {
            *mismatched = true;
            println!("\n{}:", name.bold());
        }
        for (artifact, path) in it {
            println!("  `{}`", path.display().blue());
            for (f, m) in maps.iter() {
                print!("    {:>20.20}: ", f.dimmed());
                if let Some(v) = ctx_to_item_vec(&m[artifact].0)
                    .iter()
                    .find(|i| i.path == path)
                {
                    println!(
                        "{}Bytes: {:10}, {}: {}{}",
                        "{".dimmed(),
                        v.digest.size_bytes.yellow(),
                        v.digest.hash_function_name,
                        format!("{:?}", v.digest.hash).yellow(),
                        "}".dimmed()
                    );
                } else {
                    println!("{}", "<not present>".red());
                }
            }
        }
    }

    item_mismatch_printer(
        inp,
        "Input Mismatches",
        |a| &a.inputs,
        maps,
        &mut mismatched,
    );
    item_mismatch_printer(
        out,
        "Output Mismatches",
        |a| &a.actual_outputs,
        maps,
        &mut mismatched,
    );

    if !mismatched {
        println!("{}", "No mismatches!".green());
    }
}

fn get<'l>(
    maps: &'l [(&'l String, Map<'l>)],
    path: &str,
) -> Option<Vec<(&'l String, &'l Arc<BuildAction<'l>>)>> {
    match maps
        .iter()
        .map(|(f, m)| m.get(path).map(|v| (*f, v)))
        .collect::<Option<Vec<_>>>()
    {
        Some(v) => Some(v),
        None => {
            eprintln!("`{}` not found in 1 or more execution logs", path);
            None
        }
    }
}

fn transitive_cmp<'l>(
    root: ArtifactName<'l>,
    maps: &'l [(&'l String, Map<'l>)],
) -> (
    impl Iterator<Item = (ArtifactName<'l>, &'l str)>, // env vars
    impl Iterator<Item = (ArtifactName<'l>, &'l Path)>, // inputs
    impl Iterator<Item = (ArtifactName<'l>, &'l Path)>, // outputs
) {
    let (envs, inps, outs) = (
        Mutex::new(HashMap::new()),
        Mutex::new(HashMap::new()),
        Mutex::new(HashMap::new()),
    );
    let visited = RwLock::new(HashSet::new());

    #[allow(clippy::type_complexity)]
    fn traverse<'l>(
        artifact: ArtifactName<'l>,
        (envs, inps, outs): (
            &Mutex<HashMap<&'l str, (ArtifactName<'l>, &'l str)>>,
            &Mutex<HashMap<&'l Path, (ArtifactName<'l>, &'l Path)>>,
            &Mutex<HashMap<&'l Path, (ArtifactName<'l>, &'l Path)>>,
        ),
        maps: &'l [(&'l String, Map<'l>)],
        visited: &RwLock<HashSet<ArtifactName<'l>>>,
    ) {
        if visited.read().unwrap().contains(&artifact) {
            return;
        }

        if let Some(actions) = get(maps, artifact) {
            let (env, inp, out) = find_mismatched(artifact, actions.into_iter());
            visited.write().unwrap().insert(artifact);

            envs.lock().unwrap().extend(env.map(|p| (p.1, p)));
            outs.lock().unwrap().extend(out.map(|p| (p.1, p)));

            let mismatched_inputs: Vec<_> = inp.collect();
            inps.lock()
                .unwrap()
                .extend(mismatched_inputs.iter().map(|p| (p.1, *p)));

            rayon::scope(|s| {
                for (_, path) in mismatched_inputs {
                    s.spawn(move |_| {
                        traverse(path.to_str().unwrap(), (envs, inps, outs), maps, visited)
                    });
                }
            })
        }
    }

    traverse(root, (&envs, &inps, &outs), &maps, &visited);

    (
        envs.into_inner().unwrap().into_iter().map(|(_, v)| v),
        inps.into_inner().unwrap().into_iter().map(|(_, v)| v),
        outs.into_inner().unwrap().into_iter().map(|(_, v)| v),
    )
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let args = || args_os().skip(1);
    let num_files = args().count();
    if num_files == 0 {
        return Err(eyre!("specify 1 or more files to compare!"));
    }

    let truncate_file_names = args().any(|f| f.to_str().unwrap().len() > 20)
        && args()
            .map(|f| PathBuf::from(&f).file_name().unwrap().to_owned())
            .collect::<HashSet<_>>()
            .len()
            == num_files;

    let files: Vec<(String, String)> = args()
        .progress()
        .map(PathBuf::from)
        .map(|f| {
            read_to_string(&f).map(|c| {
                let n = if truncate_file_names {
                    f.file_name().unwrap().to_str().unwrap()
                } else {
                    f.to_str().unwrap()
                };

                (c, n.to_owned())
            })
        })
        .collect::<Result<_, _>>()?;

    let p = MultiProgress::new();
    let sty = ProgressStyle::default_bar()
        .template("{msg:20!.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
        .progress_chars("#>-");
    let maps: Vec<_> = files
        .iter()
        .map(|(f, n)| {
            let pb = p.add(ProgressBar::new(f.len() as _).with_message(String::from(n)));
            pb.set_style(sty.clone());
            (f, n, pb)
        })
        .collect();
    thread::spawn(move || p.join_and_clear().unwrap());

    let maps: Vec<(_, Map)> = maps
        .par_iter()
        .map(|(f, n, p)| exec_log_to_hashmap(f.as_bytes(), p).map(|h| (*n, h)))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(n, (map, dups))| {
            if !dups.is_empty() {
                eprintln!(
                    "[{}] Some outputs in `{}` appear to be produced by multiple actions:",
                    "WARNING".yellow(),
                    n.blue()
                );
                for o in dups {
                    eprintln!("  - {}", o.underline());
                }
                eprintln!();
            }

            (n, map)
        })
        .collect();

    let mut rl = Editor::with_config(
        Config::builder()
            .auto_add_history(true)
            .completion_type(CompletionType::List)
            .completion_prompt_limit(50)
            .build(),
    );
    rl.set_helper(Some(ExecLogHelper::new(&maps)));
    let prompt = format!("{}", "> ".blue());

    loop {
        let inp = rl.readline(prompt.as_str());
        match inp.as_deref() {
            Ok("quit") | Ok("q") => break,
            Err(_) | Ok("help") => {
                println!(
                    "usage:
  - `quit` or `q` to quit
  - `cmp <output path>` to compare items of interest within the action for an output path
  - `transitive-cmp <output path>` or `tcmp` to compare all transitive dependencies of an output path
  - `edges <output path>` *attempts* to determine the inputs that caused the executions of the output path to diverge; may not be accurate
  - `diff <output path>` to print a textual diff of the fields from `view <output path>`
  - `view <output path>` to print selected fields of interest from the action for an output path"
                );

                #[cfg(feature = "json-dump-command")]
                println!("  - `json <output path>` to print the raw JSON blobs for an output path");
                println!();
            }
            #[cfg(feature = "json-dump-command")]
            Ok(path) if path.starts_with("json ") => {
                if let Some(v) = get(&maps, path.strip_prefix("json ").unwrap()) {
                    for (f, a) in v {
                        println!(
                            "`{}`:\n{}\n",
                            f.green(),
                            serde_json::to_string_pretty(&a.0)?
                        );
                    }
                }
            }
            Ok(path) if path.starts_with("cmp ") => {
                let artifact = path.strip_prefix("cmp ").unwrap();
                if let Some(v) = get(&maps, artifact) {
                    print_mismatched(find_mismatched(artifact, v.into_iter()), &maps);
                }
            }
            Ok(path) if path.starts_with("transitive-cmp ") | path.starts_with("tcmp ") => {
                let artifact = path.split_once(" ").map(|(_, a)| a).unwrap_or("");
                if get(&maps, artifact).is_none() {
                    continue;
                }

                print_mismatched(transitive_cmp(artifact, &maps), &maps);
            }
            Ok(path) if path.starts_with("edges ") => {
                let artifact = path.strip_prefix("edges ").unwrap();
                if get(&maps, artifact).is_none() {
                    continue;
                }

                let (e, i, o) = transitive_cmp(artifact, &maps);
                let i = i.collect::<Vec<_>>();
                let o = o.collect::<Vec<_>>();
                let inps = i.iter().map(|(_, i)| *i).collect::<HashSet<_>>();
                let outs = o.iter().map(|(_, o)| *o).collect::<HashSet<_>>();

                print_mismatched(
                    (
                        e,
                        i.into_iter().filter(|(_, i)| !outs.contains(i)),
                        o.into_iter().filter(|(_, o)| !inps.contains(o)),
                    ),
                    &maps,
                );
            }
            Ok(path) if path.starts_with("view ") => {
                if let Some(v) = get(&maps, path.strip_prefix("view ").unwrap()) {
                    for (f, a) in v {
                        println!("`{}`:\n{:#?}", f.green(), a.0);
                    }
                }
            }
            Ok(path) if path.starts_with("diff ") => {
                if let Some(v) = get(&maps, path.strip_prefix("diff ").unwrap()) {
                    if v.iter().all(|(_, a)| a.0 == v[0].1 .0) {
                        println!(
                            "all executions of `{}` were equivalent",
                            path.strip_prefix("diff ").unwrap()
                        );
                    } else if v.len() == 2 {
                        println!(
                            "{}",
                            prettydiff::text::diff_lines(
                                &format!("{:#?}", v[0].1 .0),
                                &format!("{:#?}", v[1].1 .0),
                            )
                        );
                    } else {
                        println!("can't diff more than 2 things yet, sorry!");
                    }
                }
            }
            _ => println!("unrecognized command!"),
        }
    }

    // Since we're exiting anyways, don't bother cleaning up memory and running
    // destructors; let the OS take care of it:
    forget(maps);

    Ok(())
}
