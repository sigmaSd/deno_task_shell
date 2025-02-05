// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use anyhow::Context;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tokio::task::JoinHandle;

use crate::execute_with_pipes;
use crate::parser::parse;
use crate::shell::fs_util;
use crate::shell::types::pipe;
use crate::shell::types::ShellPipeWriter;

// Clippy is complaining about them all having `File` prefixes,
// but there might be non-file variants in the future.
#[allow(clippy::enum_variant_names)]
enum TestAssertion {
  FileExists(String),
  FileNotExists(String),
  FileTextEquals(String, String),
}

struct TempDir {
  // hold to keep it alive until drop
  _inner: tempfile::TempDir,
  cwd: PathBuf,
}

impl TempDir {
  pub fn new() -> Self {
    let temp_dir = tempfile::tempdir().unwrap();
    let cwd = fs_util::canonicalize_path(temp_dir.path()).unwrap();
    Self {
      _inner: temp_dir,
      cwd,
    }
  }
}

pub struct TestBuilder {
  // it is much much faster to lazily create this
  temp_dir: Option<TempDir>,
  env_vars: HashMap<String, String>,
  command: String,
  stdin: Vec<u8>,
  expected_exit_code: i32,
  expected_stderr: String,
  expected_stdout: String,
  assertions: Vec<TestAssertion>,
}

impl TestBuilder {
  pub fn new() -> Self {
    let env_vars = std::env::vars()
      .into_iter()
      .map(|(key, value)| {
        // For some very strange reason, key will sometimes be cased as "Path"
        // or other times "PATH" on Windows. Since keys are case-insensitive on
        // Windows, normalize the keys to be upper case.
        if cfg!(windows) {
          // need to normalize on windows
          (key.to_uppercase(), value)
        } else {
          (key, value)
        }
      })
      .collect();

    Self {
      temp_dir: None,
      env_vars,
      command: String::new(),
      stdin: Vec::new(),
      expected_exit_code: 0,
      expected_stderr: String::new(),
      expected_stdout: String::new(),
      assertions: Vec::new(),
    }
  }

  pub fn ensure_temp_dir(&mut self) -> &mut Self {
    self.get_temp_dir();
    self
  }

  fn get_temp_dir(&mut self) -> &mut TempDir {
    if self.temp_dir.is_none() {
      self.temp_dir = Some(TempDir::new());
    }
    self.temp_dir.as_mut().unwrap()
  }

  pub fn command(&mut self, command: &str) -> &mut Self {
    self.command = command.to_string();
    self
  }

  pub fn stdin(&mut self, stdin: &str) -> &mut Self {
    self.stdin = stdin.as_bytes().to_vec();
    self
  }

  pub fn directory(&mut self, path: &str) -> &mut Self {
    let temp_dir = self.get_temp_dir();
    fs::create_dir_all(temp_dir.cwd.join(path)).unwrap();
    self
  }

  pub fn env_var(&mut self, name: &str, value: &str) -> &mut Self {
    self.env_vars.insert(name.to_string(), value.to_string());
    self
  }

  pub fn file(&mut self, path: &str, text: &str) -> &mut Self {
    let temp_dir = self.get_temp_dir();
    fs::write(temp_dir.cwd.join(path), text).unwrap();
    self
  }

  pub fn assert_exit_code(&mut self, code: i32) -> &mut Self {
    self.expected_exit_code = code;
    self
  }

  pub fn assert_stderr(&mut self, output: &str) -> &mut Self {
    self.expected_stderr.push_str(output);
    self
  }

  pub fn assert_stdout(&mut self, output: &str) -> &mut Self {
    self.expected_stdout.push_str(output);
    self
  }

  pub fn assert_exists(&mut self, path: &str) -> &mut Self {
    self.ensure_temp_dir();
    self
      .assertions
      .push(TestAssertion::FileExists(path.to_string()));
    self
  }

  pub fn assert_not_exists(&mut self, path: &str) -> &mut Self {
    self.ensure_temp_dir();
    self
      .assertions
      .push(TestAssertion::FileNotExists(path.to_string()));
    self
  }

  pub fn assert_file_equals(
    &mut self,
    path: &str,
    file_text: &str,
  ) -> &mut Self {
    self.ensure_temp_dir();
    self.assertions.push(TestAssertion::FileTextEquals(
      path.to_string(),
      file_text.to_string(),
    ));
    self
  }

  pub async fn run(&mut self) {
    let list = parse(&self.command).unwrap();
    let cwd = if let Some(temp_dir) = &self.temp_dir {
      temp_dir.cwd.clone()
    } else {
      std::env::temp_dir()
    };
    let (stdin, mut stdin_writer) = pipe();
    stdin_writer.write(&self.stdin).unwrap();
    drop(stdin_writer); // prevent a deadlock by dropping the writer
    let (stdout, stdout_handle) = get_output_writer_and_handle();
    let (stderr, stderr_handle) = get_output_writer_and_handle();

    let exit_code = execute_with_pipes(
      list,
      self.env_vars.clone(),
      &cwd,
      stdin,
      stdout,
      stderr,
    )
    .await;
    let temp_dir = if let Some(temp_dir) = &self.temp_dir {
      temp_dir.cwd.display().to_string()
    } else {
      "NO_TEMP_DIR".to_string()
    };
    assert_eq!(
      stderr_handle.await.unwrap(),
      self.expected_stderr.replace("$TEMP_DIR", &temp_dir),
      "\n\nFailed for: {}",
      self.command
    );
    assert_eq!(
      stdout_handle.await.unwrap(),
      self.expected_stdout.replace("$TEMP_DIR", &temp_dir),
      "\n\nFailed for: {}",
      self.command
    );
    assert_eq!(
      exit_code, self.expected_exit_code,
      "\n\nFailed for: {}",
      self.command
    );

    for assertion in &self.assertions {
      match assertion {
        TestAssertion::FileExists(path) => {
          assert!(
            cwd.join(&path).exists(),
            "\n\nFailed for: {}\nExpected '{}' to exist.",
            self.command,
            path,
          )
        }
        TestAssertion::FileNotExists(path) => {
          assert!(
            !cwd.join(&path).exists(),
            "\n\nFailed for: {}\nExpected '{}' to not exist.",
            self.command,
            path,
          )
        }
        TestAssertion::FileTextEquals(path, text) => {
          let actual_text = std::fs::read_to_string(cwd.join(path))
            .with_context(|| format!("Error reading {}", path))
            .unwrap();
          assert_eq!(
            &actual_text, text,
            "\n\nFailed for: {}\nPath: {}",
            self.command, path,
          )
        }
      }
    }
  }
}

fn get_output_writer_and_handle() -> (ShellPipeWriter, JoinHandle<String>) {
  let (stdout_reader, stdout_writer) = pipe();
  let stdout_handle = tokio::task::spawn_blocking(|| {
    let mut buf = Vec::new();
    stdout_reader.pipe_to(&mut buf).unwrap();
    String::from_utf8_lossy(&buf).to_string()
  });
  (stdout_writer, stdout_handle)
}
