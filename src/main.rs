use std::collections::HashMap;
use std::env::{args, args_os};
use std::fmt::{self, Debug};
use std::fs::read_to_string;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use color_eyre::eyre::{self, eyre};
use color_eyre::owo_colors::OwoColorize;
use fuzzy_matcher::{clangd::ClangdMatcher, FuzzyMatcher};
use indicatif::{MultiProgress, ProgressBar, ProgressIterator, ProgressStyle};
use rayon::prelude::*;
use rustyline::{CompletionType, Config, Editor, Helper, completion::{Candidate, Completer}, highlight::Highlighter, hint::Hinter, validate::Validator};
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
fn exec_log_to_hashmap<'l>(log: &'l [u8], pb: &ProgressBar) -> eyre::Result<Map<'l>> {
    let mut prev = 0;
    let mut curr = 0;
    let mut map = HashMap::new();

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
            assert!(map.insert(*output, action.clone()).is_none());
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

    Ok(map)
}

struct ExecLogHelper<'l> {
    map: &'l Vec<(&'l String, Map<'l>)>,
    fuzzy_matcher: ClangdMatcher,
}

impl<'l> ExecLogHelper<'l> {
    fn new(map: &'l Vec<(&'l String, Map<'l>)>) -> Self {
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

fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let files: Vec<(String, String)> = args_os()
        .zip(args())
        .skip(1)
        .progress()
        .map(|(f, n)| read_to_string(f).map(|f| (f, n)))
        .collect::<Result<_, _>>()?;

    if files.is_empty() {
        return Err(eyre!("specify 1 or more files to compare!"));
    }

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
        .collect::<Result<_, _>>()?;

    fn get<'l>(
        maps: &'l Vec<(&String, Map<'l>)>,
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
            Ok("quit") | Ok("q") => std::process::exit(0),
            Err(_) | Ok("help") => {
                println!(
                    "usage:
  - `quit` or `q` to quit
  - `cmp <output path>` to compare items of interest within the action for an output path
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
                        println!("`{}`:\n{}\n", f.green(), serde_json::to_string_pretty(&a.0)?);
                    }
                }
            }
            Ok(path) if path.starts_with("cmp ") => {
                if let Some(v) = get(&maps, path.strip_prefix("cmp ").unwrap()) {
                    let mut env_vars: HashMap<&str, (&str, usize)> = HashMap::new();
                    let mut inputs: HashMap<&Path, (&Digest, usize)> = HashMap::new();
                    let mut outputs: HashMap<&Path, (&Digest, usize)> = HashMap::new();

                    for (_, a) in v.iter() {
                        for e in a.0.environment_variables.iter() {
                            let (val, count) = env_vars.entry(e.name).or_insert((e.value, 0));
                            if *val == e.value {
                                *count += 1;
                            }
                        }

                        for i in a.0.inputs.iter() {
                            let (val, count) = inputs.entry(i.path).or_insert((&i.digest, 0));

                            if **val == i.digest {
                                *count += 1;
                            }
                        }

                        for o in a.0.actual_outputs.iter() {
                            let (val, count) = outputs.entry(o.path).or_insert((&o.digest, 0));
                            if *val == &o.digest {
                                *count += 1;
                            }
                        }
                    }

                    let mut mismatched_env_vars = env_vars.iter().filter(|(_, (_, c))| *c != v.len()).map(|(k, _)| *k).peekable();
                    let mut mismatched_inputs = inputs.iter().filter(|(_, (_, c))| *c != v.len()).map(|(k, _)| *k).peekable();
                    let mut mismatched_outputs = outputs.iter().filter(|(_, (_, c))| *c != v.len()).map(|(k, _)| *k).peekable();
                    let mut mismatched = false;

                    // Bad, inefficient, etc.
                    // TODO: DRY
                    if mismatched_env_vars.peek().is_some() {
                        mismatched = true;
                        println!("\n{}:", "Environment Variable Mismatches".bold());
                    }
                    for env_name in mismatched_env_vars {
                        println!("  ${}", env_name.blue());
                        for (f, a) in v.iter() {
                            print!("    {:>20.20}: ", f.dimmed());
                            if let Some(v) = a.0.environment_variables.iter().find(|e| e.name == env_name) {
                                println!("{}", v.value.yellow());
                            } else {
                                println!("{}", "<not present>".red());
                            }
                        }
                    }
                    if mismatched_inputs.peek().is_some() {
                        dbg!(&mismatched_inputs);
                        mismatched = true;
                        println!("\n{}:", "Input Mismatches".bold());
                    }
                    for input in mismatched_inputs {
                        println!("  `{}`", input.display().blue());
                        for (f, a) in v.iter() {
                            print!("    {:>20.20}: ", f.dimmed());
                            if let Some(v) = a.0.inputs.iter().find(|i| i.path == input) {
                                println!("{}Bytes: {:10}, {}: {}{}", "{".dimmed(), v.digest.size_bytes.yellow(), v.digest.hash_function_name, format!("{:?}", v.digest.hash).yellow(), "}".dimmed());
                            } else {
                                println!("{}", "<not present>".red());
                            }
                        }
                    }
                    if mismatched_outputs.peek().is_some() {
                        mismatched = true;
                        println!("\n{}:", "Output Mismatches".bold());
                    }
                    for output in mismatched_outputs {
                        println!("  `{}`", output.display().blue());
                        for (f, a) in v.iter() {
                            print!("    {:>20.20}: ", f.dimmed());
                            if let Some(v) = a.0.actual_outputs.iter().find(|o| o.path == output) {
                                println!("{}Bytes: {:10}, {}: {}{}", "{".dimmed(), v.digest.size_bytes.yellow(), v.digest.hash_function_name, format!("{:?}", v.digest.hash).yellow(), "}".dimmed());
                            } else {
                                println!("{}", "<not present>".red());
                            }
                        }
                    }

                    if !mismatched {
                        println!("{}", "No mismatches!".green());
                    }
                }
            },
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
                    } else {
                        if v.len() == 2 {
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
            }
            _ => println!("unrecognized command!"),
        }
    }
}
