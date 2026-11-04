# Guided model startup

**Problem:** Starting Boubou requires knowing a model ID. New users need a short
path to a suitable model; users arriving from a model library need direct access.

**Behavior:**

- Use `boubou` consistently for the executable, npm command, help and diagnostics.
  This replaces the command name in the [package contract](0006-liquid-development-package.md).
- Without a subcommand in a terminal, show an English welcome screen with exactly
  two routes: recommend a model for this Mac, or use a model already chosen.
  Use restrained magenta/cyan/yellow emphasis, default terminal text colors,
  arrow keys, Enter, Esc/back and Ctrl+C/exit. Respect `NO_COLOR`.
- Recommend at most two registered models, in curated registry order, whose
  profiled memory budget fits estimated available memory and whose cache volume
  has room for missing assets and runtime staging. Show language/task and disk
  needs. Cache presence is provisional; full asset verification happens at startup.
  The current registry contains one English voice-chat model.
- Memory fit uses a local snapshot; it does not guarantee speed or compatibility
  on every device. Exclude profiles without a memory budget from recommendations.
  The current 6 GiB budget includes headroom over approximately 2 GiB backend RSS
  observed in a bounded native startup/cancellation/next-turn check.
- The chosen-model route accepts an exact model ID, `boubou run MODEL [--offline]`,
  or the documented npx equivalent, including quoted npm-cache paths. Parse data
  only; never execute pasted shell commands. Unsupported models/options remain
  editable with a clear error. An explicitly chosen model uses the same platform,
  asset verification and startup checks as direct `run`; recommendation gates do
  not introduce a second startup policy.
- Selection restores the terminal and uses the existing verified download and
  voice-chat path. Direct `run`/`pull`/`list`/`doctor` commands bypass the welcome
  screen and retain their existing behavior. Redirected I/O and `TERM=dumb`
  print help instead of opening the menu. No npm publication is part of this change.

- After selection or direct `run`, keep startup visible: report verification,
  downloads, engine installation/loading, session connection and audio opening.
  Show measured byte percentages for verification/downloads; other phases show
  an animated bar with elapsed time and no invented percentage. Non-terminal
  output gets one phase heading without animation; retain `NO_COLOR` behavior.
  Clear active progress before errors, and show Ready only after audio opens.

**Failures:** Device detection failure or no suitable model offers a return to
the welcome screen. Browsing, cancelling and invalid input never prepare assets
or open audio devices. Restore terminal mode, cursor and signal handlers on exit
and errors. Keep existing download integrity, offline failures and process cleanup.

**Verification:** Ordinary Rust tests cover model/command parsing, resource-fit
boundaries, unknown budgets and headless CLI behavior. PTY checks cover the two
routes, keyboard navigation, English/color/no-color rendering, errors, cancellation
and terminal restoration. Run repository quality checks. The native resource
sample supports an advisory budget; it is not acceptance for every Mac, new
models, standalone text-to-speech or real microphone operation.
