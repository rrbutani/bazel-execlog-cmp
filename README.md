# `bazel-execlog-cmp`

CLI tool that helps compare Bazel execution logs.

---

## what

When trying to [debug Bazel remote caching](https://docs.bazel.build/versions/master/remote-execution-caching-debug.html#ensure-caching-across-machines), it's helpful to get [execution logs](https://docs.bazel.build/versions/master/command-line-reference.html#flag--execution_log_json_file) out of Bazel to compare (`--execution_log_json_file`).

In practice these logs can be quite massive making them difficult to compare by hand; this tool tries to help with that.

## how do i use this

First, run the tool with the paths to your execution logs:

  ```bash
  cargo run --release -- <paths to a bunch of JSON execution logs>
  ```

Then, ask it to compare the actions for the artifacts you're interested in:

  ```sh
  > cmp bazel-out/k8-opt/bin/foo.out
  Input Mismatches:
    `bazel-out/k8-opt/bin/foo.o`
          ../execlog1.json: {Bytes:       9809, SHA-256: 9316644c2e21e3f5e238ae4b503b13935d997364b711731f1955af819e983e22}
          ../execlog2.json: {Bytes:       9809, SHA-256: a34d2d7c69bdda43de87d392439232649dfe0d787c0aced1245b8ff5b342d97a}

  Output Mismatches:
    `bazel-out/k8-opt/bin/foo.out`
          ../execlog1.json: {Bytes:      16783, SHA-256: 8bc8118a9c5114910965057759b32c581d02963d2d3118f849b91ee92526d5b4}
          ../execlog2.json: {Bytes:      16782, SHA-256: 7482bd31539cb3fee803d4f0fac191d1fd96d549f8aa0808cc43df3b140b6b36}
  ```

Typically you'll start at the top (a leaf of your build graph or just an artifact that you're interested in) and then trace through the mismatched inputs recursively. For example, for the above we'd want to ask about `foo.o` next:

  ```sh
  > cmp bazel-out/k8-opt/bin/foo.o
  Environment Variable Mismatches:
    $SOME_ENV_VAR_THATS_DIFFERENT_FOR_SOME_REASON
          ../execlog1.json: hello
          ../execlog2.json: 👋

  Output Mismatches:
    `bazel-out/k8-opt/bin/foo.o`
          ../execlog1.json: {Bytes:       9809, SHA-256: 9316644c2e21e3f5e238ae4b503b13935d997364b711731f1955af819e983e22}
          ../execlog2.json: {Bytes:       9809, SHA-256: a34d2d7c69bdda43de87d392439232649dfe0d787c0aced1245b8ff5b342d97a}
  ```

Alternatively, if you'd like the full list of all the env vars/inputs/outputs that transitively differ across all the actions that were executed to build an artifact you can use `tcmp`:

  ```sh
  > tcmp bazel-out/k8-opt/bin/foo.out
  Environment Variable Mismatches:
      $SOME_ENV_VAR_THATS_DIFFERENT_FOR_SOME_REASON
            ../execlog1.json: hello
            ../execlog2.json: 👋

  Input Mismatches:
    `bazel-out/k8-opt/bin/foo.o`
          ../execlog1.json: {Bytes:       9809, SHA-256: 9316644c2e21e3f5e238ae4b503b13935d997364b711731f1955af819e983e22}
          ../execlog2.json: {Bytes:       9809, SHA-256: a34d2d7c69bdda43de87d392439232649dfe0d787c0aced1245b8ff5b342d97a}

  Output Mismatches:
    `bazel-out/k8-opt/bin/foo.o`
          ../execlog1.json: {Bytes:       9809, SHA-256: 9316644c2e21e3f5e238ae4b503b13935d997364b711731f1955af819e983e22}
          ../execlog2.json: {Bytes:       9809, SHA-256: a34d2d7c69bdda43de87d392439232649dfe0d787c0aced1245b8ff5b342d97a}
    `bazel-out/k8-opt/bin/foo.out`
        ../execlog1.json: {Bytes:      16783, SHA-256: 8bc8118a9c5114910965057759b32c581d02963d2d3118f849b91ee92526d5b4}
        ../execlog2.json: {Bytes:      16782, SHA-256: 7482bd31539cb3fee803d4f0fac191d1fd96d549f8aa0808cc43df3b140b6b36}
  ```

There are also a few other commands:

  ```sh
  > help
  usage:
    - `quit` or `q` to quit
    - `cmp <output path>` to compare items of interest within the action for an output path
    - `transitive-cmp <output path>` or `tcmp` to compare all transitive dependencies of an output path
    - `diff <output path>` to print a textual diff of the fields from `view <output path>`
    - `view <output path>` to print selected fields of interest from the action for an output path
  ```

Finally, there's also tab completion with fuzzy search; this is especially handy for output paths which can be cumbersome to type in by hand.

## anything else?

To install this instead of cloning the repo, etc.:

  ```bash
  cargo install --git https://github.com/rrbutani/bazel-execlog-cmp
  ```

This crate has one feature: `json-dump-command`. Enabling this feature unlocks the `json` command.

<details>
    <summary>An example:</summary>

  ```sh
  > json bazel-out/k8-opt/bin/foo.out
  `../execlog1.json`:
  {
    "commandArgs": ["..."],
    "environmentVariables": [{
      "name": "PATH",
      "value": "/bin:/usr/bin:/usr/local/bin"
    }, {
      "name": "PWD",
      "value": "/proc/self/cwd"
    }],
    "platform": {
      "properties": []
    },
    "inputs": [{
      "path": "bazel-out/k8-opt/bin/foo.o",
      "digest": {
        "hash": "9316644c2e21e3f5e238ae4b503b13935d997364b711731f1955af819e983e22",
        "sizeBytes": "9809",
        "hashFunctionName": "SHA-256"
      }
    }],
    "listedOutputs": ["bazel-out/k8-opt/bin/foo.out"],
    "remotable": true,
    "cacheable": true,
    "timeoutMillis": "0",
    "progressMessage": "...",
    "mnemonic": "CppCompile",
    "actualOutputs": [{
      "path": "bazel-out/k8-opt/bin/foo.out",
      "digest": {
        "hash": "8bc8118a9c5114910965057759b32c581d02963d2d3118f849b91ee92526d5b4",
        "sizeBytes": "16783",
        "hashFunctionName": "SHA-256"
      }
    }],
    "runner": "remote cache hit",
    "remoteCacheHit": true,
    "status": "",
    "exitCode": 0
  }

  `../execlog2.json`:
  {
    "commandArgs": ["..."],
    "environmentVariables": [{
      "name": "PATH",
      "value": "/bin:/usr/bin:/usr/local/bin"
    }, {
      "name": "PWD",
      "value": "/proc/self/cwd"
    }],
    "platform": {
      "properties": []
    },
    "inputs": [{
      "path": "bazel-out/k8-opt/bin/foo.o",
      "digest": {
        "hash": "a34d2d7c69bdda43de87d392439232649dfe0d787c0aced1245b8ff5b342d97a",
        "sizeBytes": "9809",
        "hashFunctionName": "SHA-256"
      }
    }],
    "listedOutputs": ["bazel-out/k8-opt/bin/foo.out"],
    "remotable": true,
    "cacheable": true,
    "timeoutMillis": "0",
    "progressMessage": "...",
    "mnemonic": "CppCompile",
    "actualOutputs": [{
      "path": "bazel-out/k8-opt/bin/foo.out",
      "digest": {
        "hash": "7482bd31539cb3fee803d4f0fac191d1fd96d549f8aa0808cc43df3b140b6b36",
        "sizeBytes": "16782",
        "hashFunctionName": "SHA-256"
      }
    }],
    "runner": "processwrapper-sandbox",
    "remoteCacheHit": false,
    "status": "",
    "exitCode": 0
  }
  ```

</details>

In contrast with the `view` command, `json` prints out every detail about the execution that produced the artifact in question. This is useful if you wish to see some of the details `view` elides, i.e. the full command that's run.

That this feature is _disabled_ by default. Note that enabling it roughly doubles the loading time this tool takes and greatly increases memory usage.

## should i use this?

I'm not sure.

I didn't realize it until after writing this but there's actually a [first party tool](https://cs.opensource.google/bazel/bazel/+/master:src/tools/execlog/) that also [tries to make it easier to compare execution logs](https://docs.bazel.build/versions/master/remote-execution-caching-debug.html#comparing-the-execution-logs).

It operates on the binary log format (protobuf instead of JSON; a fair bit smaller) but it's also a little more barebones: it gives you sorted text files that you can then compare with other tools.


