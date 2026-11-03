# clods: several Claude Code accounts per machine, across many machines

One Claude Code config dir per account, so `CLAUDE_CONFIG_DIR` picks the account. Each
profile's long-lived token (from `claude setup-token`) sits in its `settings.json`, and
onboarding is pre-completed, so there's no sign-in. Plain `claude` keeps your normal login
until you pick a default.

```
~/.clods/
  setup.sh        set up this machine's profiles from ./tokens
  push.sh         set up other machines (over ssh, or through the arena CLI)
  clods.sh        shell helpers; ~/.zshrc and ~/.bashrc source it
  fleet.example   which machine uses which account; copy it to ./fleet
  tokens          your accounts                     (yours: never overwritten, chmod 600)
  fleet           your fleet                        (yours)
  profiles/       this machine's profiles + default (this machine's)
```

clods lives in `clods/` of [arena-infra-rs](https://github.com/nickypro/arena-infra-rs), and
the `arena` binary carries a copy of it.

## Set up

```sh
arena clods install                # the scripts into ~/.clods (or: copy this folder there)
$EDITOR ~/.clods/tokens            # one "email -> sk-ant-oat01-..." per line; chmod 600
~/.clods/setup.sh                  # profiles 1..N, plus the line in ~/.zshrc / ~/.bashrc
```

Profile N is the Nth token. Add new accounts at the end, or pin a number
(`4 someone@x -> sk-ant-...`), so N is the same account on every machine. Re-run
`setup.sh` whenever `tokens` changes. `arena clods install` again updates the scripts, and
leaves `tokens`, `fleet` and `profiles/` alone.

## Use

```sh
clod2                    # claude as account 2 (any claude args work)
clods                    # list accounts; * = what plain `claude` uses here
clods default 2          # plain `claude` uses account 2, here and in new shells (or an email)
clods default none       # back to your normal login
CLAUDE_CONFIG_DIR=~/.clods/profiles/claude2 claude   # the same, from any shell or tool
```

Plain `claude` prints one dim line on stderr saying which account it's on.

## Many machines

**arena's pods:** set `CLODS_DIR` in arena's config to the folder holding `tokens` (e.g.
`~/.clods`). Then `arena pods setup`, `up` and a replace push every account to each pod they
set up, with the defaults spread (a pod that has a default keeps it), and `arena pods setup
--clods-only` pushes to running pods. Arena sends its own copy of these scripts and your
`tokens`.

**Anything else:**

```sh
./push.sh -n --spread arena9-apple arena9-bloom arena9-wish   # dry run: who gets what
./push.sh --spread arena9-apple arena9-bloom arena9-wish      # accounts 1, 2, 3 in turn
./push.sh arena9-wish=2 arena9-apple=someone@example.com      # or pick explicitly
./push.sh                                                     # or everything in ./fleet
./push.sh --arena -y apple..mayor=1                           # through `arena pods cp` + `pods run`
./push.sh -a -d 3 box1 box2                                   # every account on both, default 3
./push.sh -a -k --spread box1 box2 box3                       # the same again moves nobody
./push.sh -q -a -k --spread box1 box2                         # a line per machine
```

- **Each push sets a machine's whole state.** It installs the scripts in that machine's
  `~/.clods`, its accounts and its default. An account no longer sent to a machine loses its
  token (and any `/login` made in its profile) there, though its history stays. `machine=none` takes every token off it, `-a` or not
  (`-d none` only means no default). Re-push to rotate tokens or move machines to other
  accounts.
- **`-k` keeps the defaults machines have.** Only a machine without one gets the spread's turn
  or `-d`, so re-pushing a grown fleet doesn't move anyone's `claude` to another account (and
  away from its history).
- **Only the default's token goes**, unless you pass `-a`. Anyone who is root on a machine can
  read every token on it.
- **Nothing with a token is left behind.** Over ssh the installer goes through stdin. Through
  arena, the run step deletes the copied installer. If that step didn't run, delete
  `~/.claude-profiles-install-*.sh` on the pod yourself.
- **A re-push with nothing new writes nothing.** `setup.sh --summary` (what `push.sh -q` and
  arena read) ends with `clods: accounts=N default=K changed=0`.

## Good to know

- These tokens are inference-only, so Remote Control refuses them. `clods.sh` turns off the
  `--remote-control` default for profiles.
- An exported `ANTHROPIC_API_KEY` beats a profile token, and the notice warns about it. An
  exported `CLAUDE_CODE_OAUTH_TOKEN` (e.g. from `arena pods copy-keys`) doesn't.
- Profiles show "Claude API" and default to Sonnet: the token doesn't tell Claude Code the plan.
  Use `/model`, or set `"model"` in the profile's `settings.json`.
- A new profile copies `~/.claude/settings.json` and the folders your normal login trusts, but no
  other credential (an API key, a gateway, a key helper or a cloud provider there would beat the
  profile's token). It also symlinks `CLAUDE.md`, `agents/` and `commands/` from `~/.claude`.
- Installs on one machine take turns (`flock` on `~/.clods/.lock`), so two pushes that meet both
  finish, and the second has the last word.
- A `claude` alias or function in your rc file keeps working: `clods.sh` wraps it.

## Tests

`clods/tests/run.sh` runs `setup.sh`, `push.sh` and `clods.sh` (under bash and zsh) for real,
each test in a scratch `$HOME` with fake tokens and a fake `claude`, `ssh` and `arena`.
arena-infra-rs's CI runs them and shellcheck (`.github/workflows/clods.yml`).

## Ideas, not built

- arena could honour `fleet`'s pins (`arena9-apple 2`) when it pushes, instead of only spreading.
- The installer could pass the tokens to `setup.sh` on a file descriptor instead of a 0600 file
  in a 0700 temp dir: today a `kill -9` mid-install would leave that file behind.
- `clods` could show each account's usage, if Anthropic ever publishes a usage API for
  subscription tokens.
- New profiles start on Sonnet: a `--model` for `setup.sh` could set the profiles' model.
