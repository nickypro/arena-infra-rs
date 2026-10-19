# ARENA GPU image (`nickypro/arena-env`)

The RunPod image that `arena pods create` launches (`IMAGE=` in `config.env`). It ships:

- **Course repo:** `ARENA-education/ARENA_materials` (branch `main`) at `/root/ARENA_materials`, with a `/root/ARENA_3.0` symlink so older tooling paths still work
- **Python env:** uv venv `arena-env` at `/opt/arena-env` with CUDA torch and the repo's `requirements.txt`. zsh and bash login shells activate it automatically, and `pip` points at it
- **CPU thread cap:** `arena_cpu_threads.pth` in the venv's site-packages sets `OMP/MKL/OPENBLAS_NUM_THREADS` to the container's cgroup CPU quota at interpreter start (RunPod pods see every host core, so torch would otherwise oversubscribe and CPU ops run 10-100x slower). An explicitly set env var wins
- **Extra repos:** every external repo the chapters clone, plus `arena-llm-context`
- **Tools:** zsh + oh-my-zsh + powerlevel10k, the ARENA MOTD, VS Code settings, and the `claude`/`codex` CLIs

## Build & push

```bash
docker build -t nickypro/arena-env:9.2 docker/
# to use a different repo or branch:
#   --build-arg ARENA_REPO_ARG=owner/repo --build-arg ARENA_BRANCH_ARG=branch
docker login -u nickypro && docker push nickypro/arena-env:9.2
```

Needs ~120GB of free disk (~50GB uncompressed, ~18GB compressed on Docker Hub). The
control plane is too small, so use a throwaway Hetzner `cpx41` (US `ash`/`hil` only),
install Docker, rsync `docker/` over, then build, test, push and destroy the server.

## Smoke test

```bash
docker run --rm nickypro/arena-env:9.2 zsh -lc 'which python; pip show torch | head -2; \
  python -c "import torch, wandb, transformer_lens"; ls /root/ARENA_materials'
```
