#!/usr/bin/env bash
# apt-get install for the Ubuntu runners, the same packages a plain install gets.
#
#   .github/scripts/apt-install.sh <package>...
#
# The runners fetch from an Azure mirror first, which sometimes trickles a download for 10
# to 20 minutes (26 Oct 2026: "Fetched 22.3 MB in 21min 44s"); in October that stall was
# most of the 90th percentile of every Linux job that installs build tools. A normal run
# takes seconds, so each attempt gets a few minutes, and after a timed-out attempt the
# Azure mirror is dropped and the next one in /etc/apt/apt-mirrors.txt (archive.ubuntu.com)
# serves the same files. apt checks every file against the index hashes either way.
set -euo pipefail

mirrors=/etc/apt/apt-mirrors.txt
for attempt in 1 2 3; do
  # Download under a time limit; unpacking runs without one, so a slow mirror can never
  # interrupt dpkg.
  if sudo timeout 120 apt-get update -q &&
    sudo timeout 240 apt-get install -y -q --download-only --no-install-recommends "$@"; then
    sudo apt-get install -y -q --no-install-recommends "$@"
    exit 0
  fi
  echo "::warning::apt attempt $attempt failed or timed out; retrying without the Azure mirror"
  if [[ -f $mirrors ]] && grep -q 'azure\.archive\.ubuntu\.com' "$mirrors"; then
    sudo sed -i '/azure\.archive\.ubuntu\.com/d' "$mirrors"
    grep -q . "$mirrors" || echo 'http://archive.ubuntu.com/ubuntu/' | sudo tee "$mirrors" >/dev/null
  else
    sudo sed -i 's#http://azure\.archive\.ubuntu\.com#http://archive.ubuntu.com#g' \
      /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true
  fi
done
echo "::error::apt-get install $* failed three times"
exit 1
