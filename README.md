# crdb-harness: Test Harness for CockroachDB

This crate provides a test harness for CockroachDB, as well
as a build script which downloads a particular CockroachDB version
into the `OUT_DIR` of a target directory.

Refer to `./tools/cockroachdb_version` for the current version
downloaded by this scripts (we may make this version configurable
in the future).
