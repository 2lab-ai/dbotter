use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use tempfile::NamedTempFile;

#[derive(Debug)]
pub struct Checkpoint {
    id: &'static str,
}

#[derive(Debug, Default, Serialize)]
struct Counts {
    executed: u64,
    passed: u64,
}

#[derive(Debug, Serialize)]
struct Source<'a> {
    kind: &'static str,
    commit: &'a str,
    run_id: u64,
    run_attempt: u64,
}

#[derive(Debug, Serialize)]
struct Case<'a> {
    id: &'a str,
    executed: u64,
    passed: u64,
}

#[derive(Debug, Serialize)]
struct Document<'a> {
    schema: &'static str,
    suite: &'a str,
    test: &'a str,
    source: Source<'a>,
    started_at: &'a str,
    finished_at: &'a str,
    cases: Vec<Case<'a>>,
    measurements: &'a BTreeMap<&'static str, u64>,
}

#[derive(Debug)]
pub struct LiveEvidence {
    output: PathBuf,
    suite: &'static str,
    test: &'static str,
    source_sha: String,
    run_id: u64,
    run_attempt: u64,
    started_at: String,
    cases: BTreeMap<&'static str, Counts>,
    measurements: BTreeMap<&'static str, u64>,
}

impl LiveEvidence {
    #[allow(dead_code)]
    pub fn required(
        suite: &'static str,
        test: &'static str,
        output_environment: &str,
    ) -> io::Result<Self> {
        let output = required_environment(output_environment)?;
        let source_sha = required_environment("DBOTTER_EXPECTED_SOURCE_SHA")?;
        let run_id = required_positive_integer("GITHUB_RUN_ID")?;
        let run_attempt = required_positive_integer("GITHUB_RUN_ATTEMPT")?;
        Self::new(output, suite, test, source_sha, run_id, run_attempt)
    }

    pub fn new(
        output: impl Into<PathBuf>,
        suite: &'static str,
        test: &'static str,
        source_sha: impl Into<String>,
        run_id: u64,
        run_attempt: u64,
    ) -> io::Result<Self> {
        let source_sha = source_sha.into();
        if source_sha.len() != 40
            || !source_sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid_input(
                "source SHA must be one lowercase full Git SHA",
            ));
        }
        if run_id == 0 || run_attempt == 0 {
            return Err(invalid_input("run identity must contain positive integers"));
        }
        if !valid_identifier(suite) || !valid_identifier(test) {
            return Err(invalid_input(
                "suite and test identifiers must be stable ASCII",
            ));
        }
        let output = output.into();
        let parent = output
            .parent()
            .filter(|path| path.is_dir())
            .ok_or_else(|| invalid_input("evidence output parent must already exist"))?;
        if output.file_name().is_none() || parent.as_os_str().is_empty() {
            return Err(invalid_input("evidence output must name a file"));
        }
        Ok(Self {
            output,
            suite,
            test,
            source_sha,
            run_id,
            run_attempt,
            started_at: now(),
            cases: BTreeMap::new(),
            measurements: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn begin(&mut self, id: &'static str) -> Checkpoint {
        assert!(valid_identifier(id), "checkpoint id must be stable ASCII");
        self.cases.entry(id).or_default().executed += 1;
        Checkpoint { id }
    }

    pub fn pass(&mut self, checkpoint: Checkpoint) {
        let counts = self
            .cases
            .get_mut(checkpoint.id)
            .expect("checkpoint must originate from this recorder");
        assert!(
            counts.passed < counts.executed,
            "checkpoint cannot pass more times than it executed"
        );
        counts.passed += 1;
    }

    pub fn measure(&mut self, name: &'static str, value: usize) -> io::Result<()> {
        if !valid_identifier(name) {
            return Err(invalid_input("measurement id must be stable ASCII"));
        }
        let value = u64::try_from(value)
            .map_err(|_| invalid_input("measurement does not fit the receipt integer"))?;
        if self.measurements.insert(name, value).is_some() {
            return Err(invalid_input("measurement may be recorded only once"));
        }
        Ok(())
    }

    pub fn finish(self) -> io::Result<()> {
        if self.cases.is_empty() || self.measurements.is_empty() {
            return Err(invalid_data("evidence must contain cases and measurements"));
        }
        if self
            .cases
            .values()
            .any(|counts| counts.executed == 0 || counts.passed != counts.executed)
        {
            return Err(invalid_data("every executed checkpoint must pass"));
        }
        let finished_at = now();
        let cases = self
            .cases
            .iter()
            .map(|(id, counts)| Case {
                id,
                executed: counts.executed,
                passed: counts.passed,
            })
            .collect();
        let document = Document {
            schema: "dbotter.live-suite-evidence.v1",
            suite: self.suite,
            test: self.test,
            source: Source {
                kind: "ci_expected_sha",
                commit: &self.source_sha,
                run_id: self.run_id,
                run_attempt: self.run_attempt,
            },
            started_at: &self.started_at,
            finished_at: &finished_at,
            cases,
            measurements: &self.measurements,
        };
        publish_no_replace(&self.output, &document)
    }
}

fn publish_no_replace(output: &Path, document: &Document<'_>) -> io::Result<()> {
    let parent = output
        .parent()
        .ok_or_else(|| invalid_input("evidence output has no parent"))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), document).map_err(io::Error::other)?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[allow(dead_code)]
fn required_environment(name: &str) -> io::Result<String> {
    let value = env::var(name)
        .map_err(|_| invalid_input(&format!("required evidence variable is missing: {name}")))?;
    if value.is_empty() {
        return Err(invalid_input(&format!(
            "required evidence variable is empty: {name}"
        )));
    }
    Ok(value)
}

#[allow(dead_code)]
fn required_positive_integer(name: &str) -> io::Result<u64> {
    let value = required_environment(name)?;
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_input(&format!("{name} must be a positive integer")))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
