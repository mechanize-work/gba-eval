#!/bin/bash
# Entrypoint for the services container. Launches the oracle HTTP
# server and exits if it dies (compose restarts the container).
set -euo pipefail

# Agent (uid 1000) and services (uid 5000) are both in group gba (3000)
# and share /task. umask 002 → 0775 dirs / 0664 files so both UIDs can
# cooperate via the group.
umask 002

exec /usr/local/bin/oracle-server
