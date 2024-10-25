// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Facilities for managing a local database for development

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use camino_tempfile::tempdir;
use camino_tempfile::Utf8TempDir;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::ops::Deref;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio_postgres::config::Host;
use tokio_postgres::config::SslMode;

// A default listen port of 0 allows the system to choose any available port.
const COCKROACHDB_DEFAULT_LISTEN_PORT: u16 = 0;

/// CockroachDB database name
const COCKROACHDB_DATABASE: &str = "test";

/// CockroachDB user name
const COCKROACHDB_USER: &str = "root";

/// Path to the CockroachDB binary
const COCKROACHDB_BIN: &str = "cockroach";

/// The expected CockroachDB version
const COCKROACHDB_VERSION: &str = include_str!("../tools/cockroachdb_version");

/// Builder for [`CockroachStarter`] that supports setting some command-line
/// arguments for the `cockroach start-single-node` command
///
/// Without customizations, this will run `cockroach start-single-node --insecure
/// --listen-addr=[::1]:0 --http-addr=:0`.
///
/// It's useful to support running this concurrently (as in the test suite).  To
/// support this, we allow CockroachDB to choose its listening ports.  To figure
/// out which ports it chose, we also use the --listening-url-file option to have
/// it write the URL to a file in a temporary directory.  The Drop
/// implementations for `CockroachStarter` and `CockroachInstance` will ensure
/// that this directory gets cleaned up as long as this program exits normally.
#[derive(Debug)]
pub struct CockroachStarterBuilder {
    /// optional value for the listening port
    listen_port: u16,
    /// environment variables, mirrored here for reporting
    env: BTreeMap<String, String>,
    /// command-line arguments, mirrored here for reporting
    args: Vec<String>,
    /// describes the command line that we're going to execute
    cmd_builder: tokio::process::Command,
    /// redirect stdout and stderr to files
    redirect_stdio: bool,
}

fn cockroachdb_bin() -> Utf8PathBuf {
    // We expect that our corresponding "build.rs" build script should
    // be responsible for downloading this binary.
    Utf8Path::new(&std::env!("OUT_DIR"))
        .join("cockroachdb/bin")
        .join(COCKROACHDB_BIN)
}

impl CockroachStarterBuilder {
    pub fn new() -> CockroachStarterBuilder {
        let mut builder = CockroachStarterBuilder::new_raw(&cockroachdb_bin());

        // Copy the current set of environment variables.  We could instead
        // allow the default behavior of inheriting the current process
        // environment.  But we want to print these out.  If we use the default
        // behavior, it's possible that what we print out wouldn't match what
        // was used if some other thread modified the process environment in
        // between.
        builder.cmd_builder.env_clear();

        // Configure Go to generate a core file on fatal error.  This behavior
        // may be overridden by the user if they've set this variable in their
        // environment.
        builder.env("GOTRACEBACK", "crash");
        for (key, value) in std::env::vars_os() {
            builder.env(key, value);
        }

        // We use single-node insecure mode listening only on localhost.  We
        // consider this secure enough for development (including the test
        // suite), though it does allow anybody on the system to do anything
        // with this database (including fill up all disk space).  (It wouldn't
        // be unreasonable to secure this with certificates even though we're
        // on localhost.
        //
        // If we decide to let callers customize various listening addresses, we
        // should be careful about making it too easy to generate a more
        // insecure configuration.
        builder
            .arg("start-single-node")
            .arg("--insecure")
            .arg("--http-addr=:0");
        builder
    }

    /// Helper for constructing a `CockroachStarterBuilder` that runs a specific
    /// command instead of the usual `cockroach` binary
    ///
    /// This is used by `new()` as a starting point.  It's also used by the
    /// tests to trigger failure modes that would be hard to reproduce with
    /// `cockroach` itself.
    fn new_raw(cmd: &Utf8Path) -> CockroachStarterBuilder {
        CockroachStarterBuilder {
            listen_port: COCKROACHDB_DEFAULT_LISTEN_PORT,
            env: BTreeMap::new(),
            args: vec![cmd.to_string()],
            cmd_builder: tokio::process::Command::new(cmd),
            redirect_stdio: false,
        }
    }

    /// Redirect stdout and stderr for the "cockroach" process to files within
    /// the temporary directory.  This is used by the test suite so that people
    /// don't get reams of irrelevant output when running `cargo nextest run`.
    /// This will be cleaned up as usual on success.
    pub fn redirect_stdio_to_files(mut self) -> Self {
        self.redirect_stdio = true;
        self
    }

    fn redirect_file(
        &self,
        temp_dir_path: &Utf8Path,
        label: &str,
    ) -> Result<std::fs::File, anyhow::Error> {
        let out_path = temp_dir_path.join(label);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .with_context(|| format!("open \"{}\"", out_path))
    }

    /// Starts CockroachDB using the configured command-line arguments
    ///
    /// This will create a temporary directory for the listening URL file (see
    /// above) and potentially the database store directory (if `store_dir()`
    /// was never called).
    pub fn build(mut self) -> Result<CockroachStarter, anyhow::Error> {
        // We always need a temporary directory, if for no other reason than to
        // put the listen-url file.  (It would be nice if the subprocess crate
        // allowed us to open a pipe stream to the child other than stdout or
        // stderr, although there may not be a portable means to identify it to
        // CockroachDB on the command line.)
        //
        // TODO Maybe it would be more ergonomic to use a well-known temporary
        // directory rather than a random one.  That way, we can warn the user
        // if they start up two of them, and we can also clean up after unclean
        // shutdowns.
        let temp_dir = tempdir().with_context(|| "creating temporary directory")?;
        let store_dir = CockroachStarterBuilder::temp_path(&temp_dir, "data").into_os_string();

        // Disable the CockroachDB automatic emergency ballast file. By default
        // CockroachDB creates a 1 GiB ballast file on startup; because we start
        // many instances while running tests in parallel, this can quickly eat
        // a large amount of disk space. Disable it by setting the size to 0.
        //
        // https://www.cockroachlabs.com/docs/v21.2/cluster-setup-troubleshooting#automatic-ballast-files
        let mut store_arg = OsString::from("--store=path=");
        store_arg.push(&store_dir);
        store_arg.push(",ballast-size=0");

        let listen_url_file = CockroachStarterBuilder::temp_path(&temp_dir, "listen-url");
        let listen_arg = format!("[::1]:{}", self.listen_port);
        self.arg(&store_arg)
            .arg("--listen-addr")
            .arg(&listen_arg)
            .arg("--listening-url-file")
            .arg(listen_url_file.as_os_str());

        if self.redirect_stdio {
            let temp_dir_path = temp_dir.path();
            self.cmd_builder.stdout(Stdio::from(
                self.redirect_file(temp_dir_path, "cockroachdb_stdout")?,
            ));
            self.cmd_builder.stderr(Stdio::from(
                self.redirect_file(temp_dir_path, "cockroachdb_stderr")?,
            ));
        }

        Ok(CockroachStarter {
            temp_dir,
            listen_url_file,
            args: self.args,
            env: self.env,
            cmd_builder: self.cmd_builder,
        })
    }

    /// Convenience wrapper for self.cmd_builder.arg() that records the arguments
    /// so that we can print out the command line before we run it
    fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        let arg = arg.as_ref();
        self.args.push(arg.to_string_lossy().to_string());
        self.cmd_builder.arg(arg);
        self
    }

    /// Convenience wrapper for self.cmd_builder.env() that records the
    /// environment variables so that we can print them out before running the
    /// command
    fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, k: K, v: V) -> &mut Self {
        self.env.insert(
            k.as_ref().to_string_lossy().into_owned(),
            v.as_ref().to_string_lossy().into_owned(),
        );
        self.cmd_builder.env(k, v);
        self
    }

    /// Convenience for constructing a path name in a given temporary directory
    fn temp_path<S: AsRef<str>>(tempdir: &Utf8TempDir, file: S) -> Utf8PathBuf {
        let mut pathbuf = tempdir.path().to_owned();
        pathbuf.push(file.as_ref());
        pathbuf
    }
}

/// Manages execution of the `cockroach` command in order to start a CockroachDB
/// instance
///
/// To use this, see [`CockroachStarterBuilder`].
#[derive(Debug)]
pub struct CockroachStarter {
    /// temporary directory used for URL file and potentially data storage
    temp_dir: Utf8TempDir,
    /// path to listen URL file (inside temp_dir)
    listen_url_file: Utf8PathBuf,
    /// environment variables, mirrored here for reporting
    env: BTreeMap<String, String>,
    /// command-line arguments, mirrored here for reporting to the user
    args: Vec<String>,
    /// the command line that we're going to execute
    cmd_builder: tokio::process::Command,
}

impl CockroachStarter {
    /// Enumerates human-readable summaries of the environment variables set on
    /// execution
    pub fn environment(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env.iter().map(|(k, v)| (k.as_ref(), v.as_ref()))
    }

    /// Returns a human-readable summary of the command line to be executed
    pub fn cmdline(&self) -> impl fmt::Display {
        self.args.join(" ")
    }

    /// Returns the path to the temporary directory created for this execution
    pub fn temp_dir(&self) -> &Utf8Path {
        self.temp_dir.path()
    }

    /// Spawns a new process to run the configured command
    ///
    /// This function waits up to a fixed timeout for CockroachDB to report its
    /// listening URL.  This function fails if the child process exits before
    /// that happens or if the timeout expires.
    pub async fn start(mut self) -> Result<CockroachInstance, CockroachStartError> {
        check_db_version().await?;

        let mut child_process =
            self.cmd_builder
                .spawn()
                .map_err(|source| CockroachStartError::BadCmd {
                    cmd: self.args[0].clone(),
                    source,
                })?;
        let pid = child_process.id().unwrap();

        // Wait for CockroachDB to write out its URL information.  There's not a
        // great way for us to know when this has happened, unfortunately.  So
        // we just poll for it up to some maximum timeout.
        let wait_result = loop {
            // If CockroachDB is not running at any point in this process,
            // stop waiting for the file to become available.
            // TODO-cleanup This nastiness is because we cannot allow the
            // mutable reference to "child_process" to be part of the async
            // block.  However, we need the return value to be part of the
            // async block.  So we do the process_exited() bit outside the
            // async block.  We need to move "exited" into the async block,
            // which means anything we reference gets moved into that block,
            // which means we need a clone of listen_url_file to avoid
            // referencing "self".
            let exited = process_exited(&mut child_process);
            let listen_url_file = self.listen_url_file.clone();
            if let Some(exit_error) = exited {
                break Err(exit_error);
            }

            // When ready, CockroachDB will write the URL on which it's
            // listening to the specified file.  Try to read this file.
            // Note that its write is not necessarily atomic, so we wait
            // for a newline before assuming that it's complete.
            // TODO-robustness It would be nice if there were a version
            // of tokio::fs::read_to_string() that accepted a maximum
            // byte count so that this couldn't, say, use up all of
            // memory.
            match tokio::fs::read_to_string(&listen_url_file).await {
                Ok(listen_url) if listen_url.contains('\n') => {
                    // The file is fully written.
                    // We're ready to move on.
                    let listen_url = listen_url.trim_end();
                    let result = make_pg_config(listen_url).map_err(|source| {
                        CockroachStartError::BadListenUrl {
                            listen_url: listen_url.to_string(),
                            source,
                        }
                    });
                    break result;
                }

                Ok(_) => {
                    // The file hasn't been fully written yet.
                    // Keep waiting.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }

                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The file doesn't exist yet.
                    // Keep waiting.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }

                Err(error) => {
                    // Something else has gone wrong.  Stop immediately
                    // and report the problem.
                    let source = anyhow!(error)
                        .context(format!("checking listen file {:?}", listen_url_file));
                    break Err(CockroachStartError::Unknown { source });
                }
            }
        };

        match wait_result {
            Ok(pg_config) => Ok(CockroachInstance {
                pid,
                pg_config,
                temp_dir_path: self.temp_dir.path().to_owned(),
                temp_dir: Some(self.temp_dir),
                child_process: Some(child_process),
            }),
            Err(e) => {
                // Abort and tell the user.  We'll leave CockroachDB running so
                // the user can debug if they want.  We'll skip cleanup of the
                // temporary directory for the same reason and also so that
                // CockroachDB doesn't trip over its files being gone.
                let _preserve_directory = self.temp_dir.into_path();
                Err(e)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum CockroachStartError {
    #[error("running {cmd:?} (is the binary installed and on your PATH?)")]
    BadCmd {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "wrong version of CockroachDB installed. expected '{expected:}', \
        found: '{found:?}\nstdout:\n{stdout:}\n\nstderr:\n{stderr:}\n"
    )]
    BadVersion {
        expected: String,
        found: Result<String, anyhow::Error>,
        stdout: String,
        stderr: String,
    },

    #[error(
        "cockroach exited unexpectedly with status {exit_code} \
        (see error output above)"
    )]
    Exited { exit_code: i32 },

    #[error(
        "cockroach unexpectedly terminated by signal {signal} \
        (see error output above)"
    )]
    Signaled { signal: i32 },

    #[error("error parsing listen URL {listen_url:?}")]
    BadListenUrl {
        listen_url: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("unknown error waiting for cockroach to start")]
    Unknown {
        #[source]
        source: anyhow::Error,
    },
}

/// Manages a CockroachDB process running as a single-node cluster
///
/// You are **required** to invoke [`CockroachInstance::cleanup()`] before this object is dropped.
#[derive(Debug)]
pub struct CockroachInstance {
    /// child process id
    pid: u32,
    /// PostgreSQL config to use to connect to CockroachDB as a SQL client
    pg_config: PostgresConfig,
    /// handle to child process, if it hasn't been cleaned up already
    child_process: Option<tokio::process::Child>,
    /// handle to temporary directory, if it hasn't been cleaned up already
    temp_dir: Option<Utf8TempDir>,
    /// path to temporary directory
    temp_dir_path: Utf8PathBuf,
}

#[derive(Debug)]
pub struct PostgresConfig {
    pub parsed: tokio_postgres::config::Config,
    pub url: String,
}

impl CockroachInstance {
    /// Returns the pid of the child process running CockroachDB
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns PostgreSQL client configuration suitable for connecting to the
    /// CockroachDB database
    pub fn pg_config(&self) -> &PostgresConfig {
        &self.pg_config
    }

    /// Returns the path to the temporary directory created for this execution
    pub fn temp_dir(&self) -> &Utf8Path {
        &self.temp_dir_path
    }

    /// Returns a connection to the underlying database
    pub async fn connect(&self) -> Result<Client, tokio_postgres::Error> {
        Client::connect(&self.pg_config().parsed, tokio_postgres::NoTls).await
    }

    /// Cleans up the child process and temporary directory
    ///
    /// If the child process is still running, it will be killed with SIGKILL and
    /// this function will wait for it to exit.  Then the temporary directory
    /// will be cleaned up.
    pub async fn cleanup(&mut self) -> Result<(), anyhow::Error> {
        // SIGKILL the process and wait for it to exit so that we can remove the
        // temporary directory that we may have used to store its data.  We
        // don't care what the result of the process was.
        //
        // We use SIGKILL instead of sigterm because we don't care about
        // "cleanly exiting" - we're going to destroy the underlying datastore
        // anyway, and SIGKILL is several seconds faster than SIGTERM.
        if let Some(child_process) = self.child_process.as_mut() {
            let pid = child_process.id().expect("Missing child PID") as i32;
            let success = 0 == unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            if !success {
                bail!("Failed to send SIGKILL to DB");
            }
            child_process
                .wait()
                .await
                .context("waiting for child process")?;
            self.child_process = None;
        }

        if let Some(temp_dir) = self.temp_dir.take() {
            temp_dir
                .close()
                .context("cleaning up temporary directory")?;
        }

        Ok(())
    }
}

impl Drop for CockroachInstance {
    fn drop(&mut self) {
        // TODO-cleanup Ideally at this point we would run self.cleanup() to
        // kill the child process, wait for it to exit, and then clean up the
        // temporary directory.  However, we don't have an executor here with
        // which to run async/await code.  We could create one here, but it's
        // not clear how safe or sketchy that would be.  Instead, we expect that
        // the caller has done the cleanup already.  This won't always happen,
        // particularly for ungraceful failures.
        if self.child_process.is_some() || self.temp_dir.is_some() {
            eprintln!(
                "WARN: dropped CockroachInstance without cleaning it up first \
                (there may still be a child process running and a \
                temporary directory leaked)"
            );

            // Still, make a best effort.
            #[allow(unused_must_use)]
            if let Some(child_process) = self.child_process.as_mut() {
                child_process.start_kill();
            }
            #[allow(unused_must_use)]
            if let Some(temp_dir) = self.temp_dir.take() {
                // Do NOT clean up the temporary directory in this case.
                let path = temp_dir.into_path();
                eprintln!("WARN: temporary directory leaked: {}", path);
            }
        }
    }
}

/// Verify that CockroachDB has the correct version
pub async fn check_db_version() -> Result<(), CockroachStartError> {
    let bin = cockroachdb_bin();
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["version", "--build-tag"]);
    cmd.env("GOTRACEBACK", "crash");
    let output = cmd
        .output()
        .await
        .map_err(|source| CockroachStartError::BadCmd {
            cmd: bin.to_string(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(CockroachStartError::BadVersion {
            expected: COCKROACHDB_VERSION.trim().to_string(),
            found: Err(anyhow!(
                "error {:?} when checking CockroachDB version",
                output.status.code()
            )),
            stdout,
            stderr,
        });
    }
    let version_str = stdout.trim();

    // It's okay if the version found differs only by having the "-dirty"
    // suffix.  This check is really for catching major version mismatches.
    let version_str = if let Some(clean) = version_str.strip_suffix("-dirty") {
        clean
    } else {
        version_str
    };

    // It's okay if the version found differs only by having the "-dirty"
    // suffix.  This check is really for catching major version mismatches.
    let version_str = if let Some(clean) = version_str.strip_suffix("-dirty") {
        clean
    } else {
        version_str
    };

    if version_str != COCKROACHDB_VERSION.trim() {
        return Err(CockroachStartError::BadVersion {
            found: Ok(version_str.to_string()),
            expected: COCKROACHDB_VERSION.trim().to_string(),
            stdout,
            stderr,
        });
    }

    Ok(())
}

/// Wrapper around tokio::process::Child::try_wait() so that we can unwrap() the
/// result in one place with this explanatory comment.
///
/// The semantics of that function aren't as clear as we'd like.  The docs say:
///
/// > If the child has exited, then `Ok(Some(status))` is returned. If the
/// > exit status is not available at this time then `Ok(None)` is returned.
/// > If an error occurs, then that error is returned.
///
/// It seems we can infer that "the exit status is not available at this time"
/// means that the process has not exited.  After all, if it _had_ exited, we'd
/// fall into the first case.  It's not clear under what conditions this
/// function could ever fail.  It's not clear from the source that it's even
/// possible.
fn process_exited(child_process: &mut tokio::process::Child) -> Option<CockroachStartError> {
    child_process.try_wait().unwrap().map(interpret_exit)
}

// Returns whether the given process is currently running
#[cfg(test)]
fn process_running(pid: u32) -> bool {
    // It should be okay to invoke this syscall with these arguments.  This
    // only checks whether the process is running.
    0 == (unsafe { libc::kill(pid as libc::pid_t, 0) })
}

fn interpret_exit(exit_status: std::process::ExitStatus) -> CockroachStartError {
    if let Some(exit_code) = exit_status.code() {
        CockroachStartError::Exited { exit_code }
    } else if let Some(signal) = exit_status.signal() {
        CockroachStartError::Signaled { signal }
    } else {
        // This case should not be possible.
        CockroachStartError::Unknown {
            source: anyhow!(
                "process has an exit status, but no exit \
                        code nor signal: {:?}",
                exit_status
            ),
        }
    }
}

/// Given a listen URL reported by CockroachDB, returns a parsed
/// [`Config`] suitable for connecting to a database backed by a
/// [`CockroachInstance`].
fn make_pg_config(listen_url: &str) -> Result<PostgresConfig, anyhow::Error> {
    let pg_config = listen_url
        .parse::<tokio_postgres::Config>()
        .with_context(|| format!("parse PostgreSQL config: {:?}", listen_url))?;

    // Our URL construction makes a bunch of assumptions about the PostgreSQL
    // config that we were given.  Assert these here.  (We do not expect any of
    // this to change from CockroachDB itself, and if so, this whole thing is
    // used by development tools and the test suite, so this failure mode seems
    // okay for now.)
    let check_unsupported = vec![
        pg_config.get_application_name().map(|_| "application_name"),
        pg_config.get_connect_timeout().map(|_| "connect_timeout"),
        pg_config.get_options().map(|_| "options"),
        pg_config.get_password().map(|_| "password"),
    ];

    let unsupported_values = check_unsupported
        .into_iter()
        .flatten()
        .collect::<Vec<&'static str>>();
    if !unsupported_values.is_empty() {
        bail!(
            "unsupported PostgreSQL listen URL \
            (did not expect any of these fields: {}): {:?}",
            unsupported_values.join(", "),
            listen_url
        );
    }

    if let Some(dbname) = pg_config.get_dbname() {
        if dbname != "defaultdb" {
            // Again, we're just checking our assumptions about CockroachDB
            // here.  If we somehow found a different database name here, it'd
            // be good to understand why and whether it's correct to just
            // replace it below.
            bail!(
                "unsupported PostgreSQL listen URL (unexpected database name \
                other than \"defaultdb\"): {:?}",
                listen_url
            )
        }
    }

    // As a side note: it's rather absurd that the default configuration enables
    // keepalives with a two-hour timeout.  In most networking stacks,
    // keepalives are disabled by default.  If you enable them and don't specify
    // the idle time, you get a default two-hour idle time.  That's a relic of
    // simpler times that makes no sense in most systems today.  It's fine to
    // leave keepalives off unless configured by the consumer, but if one is
    // going to enable them, one ought to at least provide a more useful default
    // idle time.
    if !pg_config.get_keepalives() {
        bail!(
            "unsupported PostgreSQL listen URL (keepalives disabled): {:?}",
            listen_url
        );
    }

    if pg_config.get_keepalives_idle() != Duration::from_secs(2 * 60 * 60) {
        bail!(
            "unsupported PostgreSQL listen URL (keepalive idle time): {:?}",
            listen_url
        );
    }

    if pg_config.get_ssl_mode() != SslMode::Disable {
        bail!(
            "unsupported PostgreSQL listen URL (ssl mode): {:?}",
            listen_url
        );
    }
    let hosts = pg_config.get_hosts();
    let ports = pg_config.get_ports();
    assert_eq!(hosts.len(), ports.len());
    if hosts.len() != 1 {
        bail!(
            "unsupported PostgresQL listen URL \
            (expected exactly one host): {:?}",
            listen_url
        );
    }

    if let Host::Tcp(ip_host) = &hosts[0] {
        let url = format!(
            "postgresql://{}@[{}]:{}/{}?sslmode=disable",
            COCKROACHDB_USER, ip_host, ports[0], COCKROACHDB_DATABASE
        );
        let parsed = url
            .parse::<tokio_postgres::Config>()
            .with_context(|| format!("parse modified PostgreSQL config {:?}", url))?;
        Ok(PostgresConfig { parsed, url })
    } else {
        Err(anyhow!(
            "unsupported PostsgreSQL listen URL (not TCP): {:?}",
            listen_url
        ))
    }
}

/// Wraps a PostgreSQL connection and client as provided by
/// `tokio_postgres::Config::connect()`
///
/// Typically, callers of [`tokio_postgres::Config::connect()`] get back both a
/// Client and a Connection.  You must spawn a separate task to `await` on the
/// connection in order for any database operations to happen.  When the Client
/// is dropped, the Connection is gracefully terminated, its Future completes,
/// and the task should be cleaned up.  This is awkward to use, particularly if
/// you care to be sure that the task finished.
///
/// This structure combines the Connection and Client.  You can create one from a
/// [`tokio_postgres::Config`] or from an existing ([`tokio_postgres::Client`],
/// [`tokio_postgres::Connection`]) pair.  You can use it just like a
/// `tokio_postgres::Client`.  When finished, you can call `cleanup()` to drop
/// the Client and wait for the Connection's task.
///
/// If you do not call `cleanup()`, then the underlying `tokio_postgres::Client`
/// will be dropped when this object is dropped.  If there has been no connection
/// error, then the connection will be closed gracefully, but nothing will check
/// for any error from the connection.
pub struct Client {
    client: tokio_postgres::Client,
    conn_task: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
}

type ClientConnPair<S, T> = (tokio_postgres::Client, tokio_postgres::Connection<S, T>);
impl<S, T> From<ClientConnPair<S, T>> for Client
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn from((client, connection): ClientConnPair<S, T>) -> Self {
        let join_handle = tokio::spawn(connection);
        Client {
            client,
            conn_task: join_handle,
        }
    }
}

impl Deref for Client {
    type Target = tokio_postgres::Client;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Client {
    /// Invokes `config.connect(tls)` and wraps the result in a `Client`.
    pub async fn connect<T>(
        config: &tokio_postgres::config::Config,
        tls: T,
    ) -> Result<Client, tokio_postgres::Error>
    where
        T: tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>,
        T::Stream: Send + 'static,
    {
        Ok(Client::from(config.connect(tls).await?))
    }

    /// Closes the connection, waits for it to be cleaned up gracefully, and
    /// returns any error status.
    pub async fn cleanup(self) -> Result<(), tokio_postgres::Error> {
        drop(self.client);
        self.conn_task
            .await
            .expect("failed to join on connection task")
    }
}

// These are more integration tests than unit tests.
#[cfg(test)]
mod test {
    use super::make_pg_config;
    use super::process_exited;
    use super::process_running;
    use super::CockroachStartError;
    use super::CockroachStarter;
    use super::CockroachStarterBuilder;
    use camino::{Utf8Path, Utf8PathBuf};
    use std::collections::BTreeMap;
    use std::process::Stdio;
    use tokio::fs;

    fn new_builder() -> CockroachStarterBuilder {
        CockroachStarterBuilder::new().redirect_stdio_to_files()
    }

    // Tests that we clean up the temporary directory correctly when the starter
    // goes out of scope, even if we never started the instance.  This is
    // important to avoid leaking the directory if there's an error starting the
    // instance, for example.
    #[tokio::test]
    async fn test_starter_tmpdir() {
        let builder = new_builder();
        let starter = builder.build().unwrap();
        let directory = starter.temp_dir().to_owned();
        assert!(fs::metadata(&directory)
            .await
            .expect("temporary directory is missing")
            .is_dir());
        drop(starter);
        assert_eq!(
            libc::ENOENT,
            fs::metadata(&directory)
                .await
                .expect_err("temporary directory still exists")
                .raw_os_error()
                .unwrap()
        );
    }

    // Tests what happens if the "cockroach" command cannot be found.
    #[tokio::test]
    async fn test_bad_cmd() {
        let builder = CockroachStarterBuilder::new_raw(&Utf8PathBuf::from("/nonexistent"));
        let _ = test_database_start_failure(builder.build().unwrap()).await;
    }

    // Tests what happens if the "cockroach" command exits before writing the
    // listening-url file.  This looks the same to the caller (us), but
    // internally requires different code paths.
    #[tokio::test]
    async fn test_cmd_fails() {
        let mut builder = new_builder();
        builder.arg("not-a-valid-argument");
        let (temp_dir, _) = test_database_start_failure(builder.build().unwrap()).await;
        fs::metadata(&temp_dir)
            .await
            .expect("temporary directory was deleted");
        // The temporary directory is preserved in this case so that we can
        // debug the failure.  In this case, we injected the failure.  Remove
        // the directory to avoid leaking it.
        //
        // We could use `fs::remove_dir_all`, but if somehow `temp_dir` was
        // incorrect, we could accidentally do a lot of damage.  Instead, remove
        // just the files we expect to be present, and then remove the
        // (now-empty) directory.
        fs::remove_file(temp_dir.join("cockroachdb_stdout"))
            .await
            .expect("failed to remove cockroachdb stdout file");
        fs::remove_file(temp_dir.join("cockroachdb_stderr"))
            .await
            .expect("failed to remove cockroachdb stderr file");
        fs::remove_dir(temp_dir)
            .await
            .expect("failed to remove cockroachdb temp directory");
    }

    // Helper function for testing cases where the database fails to start.
    // Returns the temporary directory used by the failed attempt so that the
    // caller can decide whether to check if it was cleaned up or not.  The
    // expected behavior depends on the failure mode.
    async fn test_database_start_failure(
        starter: CockroachStarter,
    ) -> (Utf8PathBuf, CockroachStartError) {
        let temp_dir = starter.temp_dir().to_owned();
        eprintln!("will run: {}", starter.cmdline());
        eprintln!("environment:");
        for (k, v) in starter.environment() {
            eprintln!("    {}={}", k, v);
        }
        let error = starter
            .start()
            .await
            .expect_err("unexpectedly started database");
        eprintln!("error: {:?}", error);
        (temp_dir, error)
    }

    // Test the happy path using the default store directory.
    #[tokio::test]
    async fn test_setup_database_default_dir() {
        let starter = new_builder().build().unwrap();

        // In this configuration, the database directory should exist within the
        // starter's temporary directory.
        let data_dir = starter.temp_dir().join("data");

        // This common function will verify that the entire temporary directory
        // is cleaned up.  We do not need to check that again here.
        test_setup_database(starter, &data_dir, &BTreeMap::new()).await;
    }

    // Test the happy path: start the database, run a query against the URL we
    // found, and then shut it down cleanly.
    async fn test_setup_database<P: AsRef<Utf8Path>>(
        starter: CockroachStarter,
        data_dir: P,
        env_overrides: &BTreeMap<&str, &str>,
    ) {
        eprintln!("will run: {}", starter.cmdline());
        eprintln!("environment:");
        for (k, v) in starter.environment() {
            eprintln!("    {}={}", k, v);
        }

        // Figure out the expected environment by starting with the hardcoded
        // environment (GOTRACEBACK=crash), override with values from the
        // current environment, and finally override with values applied by the
        // caller.
        let vars = std::env::vars().collect::<Vec<_>>();
        let env_expected = {
            let mut env = BTreeMap::new();
            env.insert("GOTRACEBACK", "crash");
            for (k, v) in &vars {
                env.insert(k, v);
            }
            for (k, v) in env_overrides {
                env.insert(k, v);
            }
            env
        };

        // Compare the configured environment against what we expected.
        assert_eq!(env_expected, starter.environment().collect());

        // Start the database.
        let mut database = starter.start().await.expect("failed to start database");
        let pid = database.pid();
        let temp_dir = database.temp_dir().to_owned();

        // The database process should be running and the database's store
        // directory should exist.
        assert!(process_running(pid));
        assert!(fs::metadata(data_dir.as_ref())
            .await
            .expect("CockroachDB data directory is missing")
            .is_dir());

        // Check the environment variables.  Doing this is platform-specific and
        // we only bother implementing it for illumos.
        #[cfg(target_os = "illumos")]
        verify_environment(&env_expected, pid).await;

        // Try to connect to it and run a query.
        eprintln!("connecting to database");
        let client = database
            .connect()
            .await
            .expect("failed to connect to newly-started database");

        let row = client
            .query_one("SELECT 12345", &[])
            .await
            .expect("basic query failed");
        assert_eq!(row.len(), 1);
        assert_eq!(row.get::<'_, _, i64>(0), 12345);

        client
            .cleanup()
            .await
            .expect("connection unexpectedly failed");
        database
            .cleanup()
            .await
            .expect("failed to clean up database");

        // Check that the database process is no longer running.
        assert!(!process_running(pid));

        // Check that the temporary directory used by the starter has been
        // cleaned up.
        assert_eq!(
            libc::ENOENT,
            fs::metadata(&temp_dir)
                .await
                .expect_err("temporary directory still exists")
                .raw_os_error()
                .unwrap()
        );

        eprintln!("cleaned up database and temporary directory");
    }

    #[cfg(target_os = "illumos")]
    async fn verify_environment(env_expected: &BTreeMap<&str, &str>, pid: u32) {
        use std::io::BufRead;

        // Run `pargs -e PID` to dump the environment.
        let output = tokio::process::Command::new("pargs")
            .arg("-e")
            .arg(format!("{}", pid))
            .output()
            .await
            .expect("`pargs -e` failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        eprintln!(
            "pargs -e {} -> {:?}:\nstderr = {}\nstdout = {}",
            pid, output.status, stdout, stderr
        );
        assert!(output.status.success(), "`pargs -e` unexpectedly failed");

        // Buffer the output and parse it.
        let lines = std::io::BufReader::<&[u8]>::new(output.stdout.as_ref())
            .lines()
            .map(|l| l.unwrap().replace("\\\\", "\\"))
            .filter(|l| l.starts_with("envp["))
            .collect::<Vec<_>>();
        let mut env_found: BTreeMap<&str, &str> = lines
            .iter()
            .map(|line| {
                let (_, envpart) = line.split_once("]: ").expect("`pargs -e` garbled");
                envpart.split_once('=').expect("`pargs -e` garbled")
            })
            .collect();

        // Compare it to what we expect.
        let mut okay = true;
        for (expected_key, expected_value) in env_expected {
            match env_found.remove(expected_key) {
                Some(found_value) => {
                    // We ignore non-ASCII values because `pargs` encodes these
                    // in a way that's annoying for us to parse.  The purpose of
                    // this test is to catch variables that are wholly wrong,
                    // not to catch corruption within the string (which seems
                    // exceedingly unlikely).
                    if expected_value.is_ascii()
                        && !expected_value.chars().any(|c| c.is_ascii_control())
                        && found_value != *expected_value
                    {
                        okay = false;
                        println!(
                            "error: mismatched value for env var {:?}: \
                            expected {:?}, found {:?})",
                            expected_key, expected_value, found_value
                        );
                    }
                }
                None => {
                    okay = false;
                    println!("error: missing expected env var: {:?}", expected_key);
                }
            }
        }

        for (expected_key, _) in env_found {
            okay = false;
            println!("error: found unexpected env var: {:?}", expected_key);
        }

        if !okay {
            panic!("environment mismatch (see above)");
        }
    }

    // Test that you can run the database twice concurrently (and have different
    // databases!).
    #[tokio::test]
    async fn test_database_concurrent() {
        let mut db1 = new_builder()
            .build()
            .expect("failed to create starter for the first database")
            .start()
            .await
            .expect("failed to start first database");
        let mut db2 = new_builder()
            .build()
            .expect("failed to create starter for the second database")
            .start()
            .await
            .expect("failed to start second database");
        let client1 = db1
            .connect()
            .await
            .expect("failed to connect to first database");
        let client2 = db2
            .connect()
            .await
            .expect("failed to connect to second database");

        client1
            .batch_execute("CREATE DATABASE d; use d; CREATE TABLE foo (v int)")
            .await
            .expect("create (1)");
        client2
            .batch_execute("CREATE DATABASE d; use d; CREATE TABLE foo (v int)")
            .await
            .expect("create (2)");
        client1
            .execute("INSERT INTO foo VALUES (5)", &[])
            .await
            .expect("insert");
        client1
            .cleanup()
            .await
            .expect("first connection closed ungracefully");

        let rows = client2
            .query("SELECT v FROM foo", &[])
            .await
            .expect("list rows");
        assert_eq!(rows.len(), 0);
        client2
            .cleanup()
            .await
            .expect("second connection closed ungracefully");

        db1.cleanup()
            .await
            .expect("failed to clean up first database");
        db2.cleanup()
            .await
            .expect("failed to clean up second database");
    }

    #[test]
    fn test_make_pg_config_fail() {
        // failure to parse initial listen URL
        let error = make_pg_config("").unwrap_err().to_string();
        eprintln!("found error: {}", error);
        assert!(error.contains("unsupported PostgreSQL listen URL"));

        // unexpected contents in initial listen URL (wrong db name)
        let error = make_pg_config("postgresql://root@[::1]:45913/foobar?sslmode=disable")
            .unwrap_err()
            .to_string();
        eprintln!("found error: {}", error);
        assert!(error.contains(
            "unsupported PostgreSQL listen URL \
            (unexpected database name other than \"defaultdb\"): "
        ));

        // unexpected contents in initial listen URL (extra param)
        let error = make_pg_config("postgresql://root@[::1]:45913/foobar?application_name=foo")
            .unwrap_err()
            .to_string();
        eprintln!("found error: {}", error);
        assert!(error.contains(
            "unsupported PostgreSQL listen URL \
            (did not expect any of these fields: application_name)"
        ));
    }

    // Tests the way `process_exited()` checks and interprets the exit status
    // for normal process termination.
    #[tokio::test]
    async fn test_process_exit_normal() {
        // Launch a process that exits with a known status code.
        let mut child_process = tokio::process::Command::new("bash")
            .args(&["-c", "exit 3"])
            .spawn()
            .expect("failed to invoke bash");
        let pid = child_process.id().unwrap();
        println!("launched child process {}", pid);

        // The only way we have to wait for the process to exit also consumes
        // its exit status.
        let result = child_process
            .wait()
            .await
            .expect("failed to wait for child process completion");
        let exit_status = super::interpret_exit(result);

        println!("process exited: {:?}", exit_status);
        assert!(matches!(exit_status,
            CockroachStartError::Exited { exit_code } if exit_code == 3
        ));
    }

    // Tests the way `process_exited()` checks and interprets the exit status
    // for abnormal process termination (by a signal).
    #[tokio::test]
    async fn test_process_exit_abnormal() {
        // Launch a process that will hang until we can kill it with a known
        // signal.
        let mut child_process = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .expect("failed to invoke cat");
        let pid = child_process.id().unwrap();
        println!("launched child process {}", pid);

        // The child must not have exited yet because it's waiting on stdin.
        let exited = process_exited(&mut child_process);
        assert!(exited.is_none());

        // Kill the child process with a known signal.
        child_process.start_kill().unwrap();

        // The only way we have to wait for the process to exit also consumes
        // its exit status.
        let result = child_process
            .wait()
            .await
            .expect("failed to wait for child process completion");
        let exit_status = super::interpret_exit(result);

        println!("process exited: {:?}", exit_status);
        assert!(matches!(exit_status,
            CockroachStartError::Signaled { signal } if signal == libc::SIGKILL
        ));
    }
}
