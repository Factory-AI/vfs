# Testing AgentFS

## pjdfstest

AgentFS keeps two pjdfstest modes:

- `phase45-ci`: a conservative, unprivileged supported subset that should pass on the current FUSE implementation.
- `phase5-ci`: the expanded Phase 5 unprivileged supported subset. It includes `phase45-ci` plus additional currently-passing path, FIFO, symlink-loop, sparse large-file, socket-open, and rename/rmdir error-path coverage.
- `full`: the upstream pjdfstest suite, used for exploratory POSIX triage and Phase 5 planning.

The supported subset intentionally excludes tests that require root-only capabilities (`mknod` for block/char devices, successful `chown`/`lchown`, and alternate uid/gid execution). Those exclusions are tracked in `scripts/validation/posix/pjdfstest/known-gaps.tsv` so Phase 5 can separate unsupported-by-contract gaps from real filesystem bugs.

### Install pjdfstest locally

```bash
git clone https://github.com/pjd/pjdfstest.git
cd pjdfstest
autoreconf -ifs
./configure --prefix="$HOME/.local"
make pjdfstest
install -m 0755 pjdfstest "$HOME/.local/bin/pjdfstest"
command -v prove
command -v pjdfstest
```

### Run the supported AgentFS gate

Build the CLI first if needed:

```bash
cd cli
cargo build
cd ..
```

Then run the Phase 4.5 supported profile:

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase45-ci
```

Expected result:

```text
All tests successful.
Files=37, Tests=142
Result: PASS
```

The harness writes a report directory containing:

- `pjdfstest.log` - TAP output from `prove`
- `status.txt` - `prove` exit status
- `selected-profile.txt` - selected profile name
- `selected-tests.txt` - exact test files run
- `known-unsupported.tsv` - current known POSIX gaps and triage rationale

Missing prerequisites exit with `77`. A nonzero exit other than `77` means the selected supported profile failed and should be treated as a real regression.

List supported profiles:

```bash
scripts/validation/posix/run-pjdfstest.sh --list-profiles
```

Run the expanded Phase 5 supported profile:

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase5-ci
```

Summarize a pjdfstest report and map failures to the known-gap taxonomy:

```bash
scripts/validation/posix/summarize-pjdfstest-log.py \
  /path/to/pjdfstest.log \
  --known-gaps scripts/validation/posix/pjdfstest/known-gaps.tsv
```

### Run the full exploratory suite

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile full
```

Full pjdfstest currently exposes known AgentFS POSIX gaps. Use it to expand `known-gaps.tsv` and to choose the next Phase 5 correctness work; do not use `full` as a required CI pass gate until the gaps are resolved or explicitly declared unsupported.

## xftests

First, build the `agentfs` executable and install it locally including the `mount.fuse.agentfs` helper:

```bash
cd cli
cargo build --release
cp target/release/agentfs /usr/local/bin
cp scripts/mount.fuse.agentfs /sbin
```

Then, clone the xfstests repo:

```bash
git clone git://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git
```

Configure the filesystem under test:

```bash
cat local.config
export FSTYP=fuse
export FUSE_SUBTYP=.agentfs
export TEST_DEV=<database file>
export TEST_DIR=<mount directory>
```

Then, run xfstests:

```bash
sudo ./check -g quick generic/
```
