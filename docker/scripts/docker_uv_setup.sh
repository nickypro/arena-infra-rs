#!/usr/bin/env bash
# Same provisioning as the CLI's bare-VM setup, with CUDA wheels and no VM services.
set -euo pipefail
ARENA_REPO="${1:-ARENA-education/ARENA_materials}"
export ARENA_BRANCH="${2:-main}"
export REPO_DIR="${REPO_DIR:-$HOME/${ARENA_REPO##*/}}"
export REPO_URL="https://github.com/$ARENA_REPO.git"
export ARENA_CONTAINER=1 ARENA_GPU=1
export ARENA_TORCH_BACKEND="${ARENA_TORCH_BACKEND:-cu130}"
export INFRA_DIR="${INFRA_DIR:-$HOME/.arena_infra}"
source "$INFRA_DIR/scripts/arena_provision.sh"
arena_provision
