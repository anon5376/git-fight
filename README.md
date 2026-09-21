# git fight

Resolve merge conflicts by beating up the other branch.

<!-- Record the demo with: vhs demo.tape -->
![Two ASCII fighters in a terminal fighting over a merge conflict](demo.gif)

git fight is a git merge tool with a fighting game built in. Each conflict in a file is one round. Your branch fights on the left and the incoming branch on the right. The winner's version of the code goes into the file. A draw leaves the conflict as it was.

## Install

You need git and a Rust toolchain.

```bash
cargo install --git https://github.com/YOUR_USERNAME/git-fight
git fight install --apply
```

The second command makes git fight your default merge tool by setting three values in your global git config:

```bash
git config --global mergetool.fight.cmd 'git-fight "$BASE" "$LOCAL" "$REMOTE" "$MERGED"'
git config --global mergetool.fight.trustExitCode true
git config --global merge.tool fight
```

Run `git fight install` without `--apply` to print these commands instead of running them.

## Use

When a merge, rebase or cherry-pick stops on conflicts, run either of these:

```bash
git mergetool   # git opens git fight once per conflicted file
git fight       # git fight finds the conflicted files itself
```

You pick a mode at the start: fight the CPU, or play 2 players on one keyboard.

## Online

Comment `/fight` on a GitHub pull request that has merge conflicts. The GitHub App challenges the two colliding authors to a live match in the browser. Teammates can watch. Each round decides one conflict. After the last round the bot pushes the resolution to a **new** `git-fight/pr-<number>-<match-id>` branch for humans to review. It never force-pushes, never writes an existing branch, and never merges the PR. A draw (or any unresolved conflict) skips the whole push.

Install and configure the App: [SETUP.md](SETUP.md).

Play locally in the browser (demo, vs CPU, 2 players, or a hosted lockstep match) from the same `web/` client the server serves.

**Leaderboard** for a repo:

`https://<your-host>/<owner>/<repo>/leaderboard`

**README badge** (shields-style SVG, wins for that login in that repo):

```markdown
![git fight](https://<your-host>/badge/<owner>/<repo>/<login>)
```

Replace `<your-host>` with the public URL of your `git-fight-server`. Replace owner, repo, and login with the GitHub names.

## Controls

| Move | Player 1 (left) | Player 2 (right) |
|---|---|---|
| Punch | `a` | `j` |
| Kick | `s` | `k` |
| Block | `d` | `l` |
| Special | `f` | `;` |

Against the CPU, you use the Player 1 keys whichever side you picked.

Terminals can't tell when a key is released, so every move is a single tap. Block raises your guard for about half a second.

Press `Tab` during a round to skip the fight and pick the code yourself. Press `q` to quit. Quitting writes nothing, including rounds you already won.

## Winning a round

Knock out the other fighter, or have more HP when the timer runs out. An exact tie is a draw, and that conflict stays in the file for you to fix by hand.

## Fighter stats

Each fighter is built from the repo's history:

| Stat | Where it comes from |
|---|---|
| Name | The author of the latest commit on that side that touched the file |
| HP | Their share of the file's lines in `git blame`, scaled to 80–120 |
| Armor | 10% less damage taken if their commit also changed a test file |
| Special move | Unlocked if they committed on at least 3 different days in the past week |

If git can't answer one of these, that fighter gets the default.

During a rebase, git swaps what "ours" and "theirs" mean, so your own commit fights on the right. The names on the fighters are still correct.

## Safety

- A draw, a skip or a quit leaves the conflict markers exactly as they were.
- git fight writes to a temporary file and then moves it into place, so a crash can't leave half a file.
- git fight never stages or commits. `git mergetool` stages the file only when every conflict in it was resolved.
- Binary files and broken conflict markers are refused, and the file is left untouched.
- A property test feeds random text through the conflict parser and checks that, with no side picked, every byte comes back unchanged.

## Not in the mood

`--no-fight` turns git fight into a plain merge tool: both versions side by side, one conflict at a time.

| Key | Action |
|---|---|
| `1` | Keep ours |
| `2` | Keep theirs |
| `b` | Keep both, ours first |
| `s` | Skip and leave the conflict in the file |
| `u` | Undo the last choice |
| `q` | Quit without writing |

For scripts, `--pick ours`, `--pick theirs` or `--pick both` resolves every conflict without a UI.

## FAQ

**Does winning mean my code is right?**
It means you won. Build and run the tests after the merge like you normally would.

**Can I settle a real argument with a coworker this way?**
That's what 2-player mode is for.

## Uninstall

```bash
git config --global --unset merge.tool
git config --global --remove-section mergetool.fight
cargo uninstall git-fight
```

## License

MIT
