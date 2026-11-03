"""arena pods test: the course env's notebook kernel. Run by the env's own python (argv[1]), it
lists the Jupyter kernels that run that python (an ipykernel, its `bin/python`), starts one in the
script's temp dir, runs `import torch` in it, checks it is that env (`sys.prefix`) and stops it.
Prints only `kernel.<fact>=<value>` lines (the deep check's contract). The kernel's pid goes to
argv[2], for the script to kill its process group whatever happens here; the file is removed once
the kernel is shut down. Its runtime files and IPython's live in the temp dir (JUPYTER_RUNTIME_DIR,
IPYTHONDIR, set into the kernel's own environment over whatever its spec says): nothing of the
participant's is written."""

import os
import signal
import sys
import time

START = 20.0  # seconds for the kernel to answer
RUN = 25.0  # seconds for `import torch` in it


def one(v, n=300):
    return " ".join(str(v).split())[:n]


def kv(k, v):
    print(f"kernel.{k}={one(v)}", flush=True)


def err(e):
    return f"error: {type(e).__name__}: {e}"


# What the kernel gets in its environment whatever its spec says: the temp dir's, and no bytecode.
ISOLATION = ("JUPYTER_RUNTIME_DIR", "IPYTHONDIR", "PYTHONDONTWRITEBYTECODE")


def runs_this_python(argv, bindir):
    """A spec runs this env's ipykernel: argv[0] is a python in this env's `bin` (the directory
    compared, never the interpreter it links to — two venvs share one), or a bare `python`
    jupyter_client launches as the python that starts it (this one); and it starts ipykernel."""
    exe = argv[0] if argv else ""
    bare = exe in ("python", "python3", f"python{sys.version_info[0]}.{sys.version_info[1]}")
    ours = os.path.isabs(exe) and os.path.realpath(os.path.dirname(exe)) == bindir
    return (bare or ours) and any(a.startswith("ipykernel") for a in argv[1:])


def main():
    envpy, pidfile = sys.argv[1], sys.argv[2]
    try:
        import ipykernel
    except BaseException as e:
        kv("ipykernel", err(e))
        return
    kv("ipykernel", "yes")
    try:
        from jupyter_client.kernelspec import KernelSpecManager
        from jupyter_client.manager import KernelManager
    except BaseException as e:
        kv("specs", err(e))
        return
    bindir = os.path.realpath(os.path.dirname(envpy))
    prefix = os.path.realpath(os.path.dirname(bindir))
    builtin = os.path.dirname(os.path.realpath(ipykernel.__file__))
    try:
        specs = KernelSpecManager().get_all_specs()
    except BaseException as e:
        kv("specs", err(e))
        return
    mine, registered = [], []
    for name, s in sorted(specs.items()):
        argv = (s.get("spec") or {}).get("argv") or []
        if not runs_this_python(argv, bindir):
            continue
        mine.append(name)
        # Registered: a kernel.json on disk (the user's, the system's, or the env's own share dir,
        # where pip puts ipykernel's) — not the one jupyter_client makes up from ipykernel's own
        # resources when there is none.
        where = os.path.realpath(s.get("resource_dir") or "")
        if not where.startswith(builtin):
            registered.append(name)
    kv("specs", ",".join(mine) or "none")
    kv("registered", ",".join(registered) or "none")
    if not mine:
        return
    name = (registered or mine)[0]
    kv("name", name)
    km = KernelManager(kernel_name=name)
    # The spec's own `env` is laid over ours at launch: the isolation goes into it, last.
    spec = km.kernel_spec
    spec.env = {**(spec.env or {}), **{k: os.environ[k] for k in ISOLATION if k in os.environ}}
    kc = None
    t0 = time.monotonic()
    try:
        try:
            km.start_kernel(cwd=os.environ.get("JUPYTER_RUNTIME_DIR") or None)
            pid = getattr(getattr(km, "provisioner", None), "pid", None) or getattr(getattr(km, "kernel", None), "pid", None)
            if pid:
                with open(pidfile, "w") as f:
                    f.write(str(pid))
            kc = km.client()
            kc.start_channels()
            kc.wait_for_ready(timeout=START)
        except BaseException as e:
            words = str(e).lower()
            timed_out = "didn't respond" in words or "timed out" in words or "timeout" in words
            kv("start", f"timed out after {START:.0f}s" if timed_out else err(e))
            return
        kv("start_secs", f"{time.monotonic() - t0:.1f}")
        out = []

        def hook(msg):
            if msg.get("msg_type") == "stream":
                out.append(msg.get("content", {}).get("text", ""))

        t1 = time.monotonic()
        try:
            reply = kc.execute_interactive(
                "import os, sys, torch; print(torch.__version__); print(os.path.realpath(sys.prefix))",
                timeout=RUN,
                output_hook=hook,
                store_history=False,
                allow_stdin=False,
            )
            c = reply.get("content", {})
            said = "".join(out).split()
            if c.get("status") == "ok":
                kv("torch", "ok")
                kv("torch_version", said[0] if said else "?")
                kv("torch_secs", f"{time.monotonic() - t1:.1f}")
                if len(said) > 1 and said[1] != prefix:
                    kv("prefix", said[1])
            else:
                kv("torch", f"error: {c.get('ename', '?')}: {c.get('evalue', '')}")
        except TimeoutError:
            kv("torch", f"timed out after {RUN:.0f}s")
        except BaseException as e:
            kv("torch", err(e))
    finally:
        try:
            if kc is not None:
                kc.stop_channels()
            km.shutdown_kernel(now=True)
            if not km.is_alive():
                os.remove(pidfile)
        except BaseException:
            pass


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    import warnings

    warnings.simplefilter("ignore")
    main()
    kv("done", 1)
