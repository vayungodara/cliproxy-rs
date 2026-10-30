#!/bin/sh
# Nextest setup script: an empty directory for this run's build of the Go test plugins
# (cpa_plugin::testing::built_plugins), so the run's test processes build them once and
# no build outlives the run.
set -eu
dir="$PWD/target/nextest-goplugins"
rm -rf "$dir"
mkdir -p "$dir"
echo "CPA_GOPLUGINS_DIR=$dir" >> "$NEXTEST_ENV"
