#!/usr/bin/env bash
#
# Install the dabba DockerBackend reconcile loop for THIS host.
#   macOS  -> a launchd LaunchAgent (StartInterval 60, RunAtLoad) that runs
#             reconcile.sh once a minute, logging to a file.
#   Linux  -> systemd USER units (gitops-reconcile.service + .timer) enabled
#             with `systemctl --user enable --now`.
# The platform is chosen by `uname`; uninstall.sh reverses whichever ran.
#
# Configure via env vars (same ones reconcile.sh reads), all optional:
#   GITOPS_DIR       path to the gitops clone    (default ~/dabba-gitops)
#   STACKS_DIR       where stacks are applied     (default ~/stacks)
#   BOX_NAME         this box's directory name    (default `hostname -s`)
#   GITOPS_APPS_DIR  apps subtree in the repo     (default apps)
#   GITOPS_BRANCH    branch to track              (default main)
#   DABBA_ENVIRONMENT  which dabba env this loop serves (default "default")
#
# The scheduler identity is derived from DABBA_ENVIRONMENT, so two docker-host
# environments on one box each get their own loop. It used to be a fixed label:
# bringing up a second environment silently replaced the first one's agent, and
# tearing either one down stopped both.
#
# Every one of these is baked into the installed unit, so the scheduled ticks
# run with the same configuration as the install-time pass. (They did not
# always: GITOPS_APPS_DIR used to be honoured here and then silently dropped by
# the unit templates, so a non-default appsDir appeared to work and did nothing.)
#
# The gitops clone must already exist (git clone the gitops repo to GITOPS_DIR
# first); this installer only wires up the loop. Portable bash 3.2 / GNU bash.
set -euo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")" && pwd)"
RECONCILE_SH="$BACKEND_DIR/reconcile.sh"

GITOPS_DIR="${GITOPS_DIR:-$HOME/dabba-gitops}"
STACKS_DIR="${STACKS_DIR:-$HOME/stacks}"
BACKUPS_DIR="${BACKUPS_DIR:-$HOME/backups}"
BOX_NAME="${BOX_NAME:-$(hostname -s)}"
GITOPS_APPS_DIR="${GITOPS_APPS_DIR:-apps}"
GITOPS_BRANCH="${GITOPS_BRANCH:-main}"
DABBA_ENVIRONMENT="${DABBA_ENVIRONMENT:-default}"
OPENBAO_PROJECT="${OPENBAO_PROJECT:-gitops-openbao}"
OPENBAO_TOKEN_FILE="${OPENBAO_TOKEN_FILE:-}"
PLATFORM="$(uname -s)"

# Scheduler identity, per environment. Kept in one place so install.sh,
# uninstall.sh and the Rust status probe cannot drift apart.
LABEL="io.spicelabs.dabba.reconcile.$DABBA_ENVIRONMENT"
UNIT_BASE="gitops-reconcile-$DABBA_ENVIRONMENT"
# The scheduled-job failure alert, also per environment. A single box-wide unit
# would be simpler, but then tearing down one environment either removes alerting
# for the others or leaves a unit behind that nothing owns.
JOB_ALERT="$UNIT_BASE-job-alert@"

chmod +x "$RECONCILE_SH" "$BACKEND_DIR/reconcile-alert.sh" \
         "$BACKEND_DIR/reconcile-with-alerting.sh" 2>/dev/null || true

if [ ! -d "$GITOPS_DIR/.git" ]; then
    echo "WARN: $GITOPS_DIR is not a git clone yet — clone the gitops repo there"
    echo "      before the loop can converge anything (installing the loop anyway)."
fi

# Render a template file, substituting the __TOKENS__, to a destination path.
# '|' is the sed delimiter; none of the substituted paths contain it.
render() {
    local src="$1" dest="$2"
    sed \
        -e "s|__RECONCILE_SH__|$RECONCILE_SH|g" \
        -e "s|__BACKEND_DIR__|$BACKEND_DIR|g" \
        -e "s|__GITOPS_DIR__|$GITOPS_DIR|g" \
        -e "s|__STACKS_DIR__|$STACKS_DIR|g" \
        -e "s|__BACKUPS_DIR__|$BACKUPS_DIR|g" \
        -e "s|__BOX_NAME__|$BOX_NAME|g" \
        -e "s|__GITOPS_APPS_DIR__|$GITOPS_APPS_DIR|g" \
        -e "s|__GITOPS_BRANCH__|$GITOPS_BRANCH|g" \
        -e "s|__DABBA_ENVIRONMENT__|$DABBA_ENVIRONMENT|g" \
        -e "s|__OPENBAO_PROJECT__|$OPENBAO_PROJECT|g" \
        -e "s|__OPENBAO_TOKEN_FILE__|$OPENBAO_TOKEN_FILE|g" \
        -e "s|__LABEL__|$LABEL|g" \
        -e "s|__UNIT_BASE__|$UNIT_BASE|g" \
        -e "s|__JOB_ALERT__|$JOB_ALERT|g" \
        -e "s|__PATH__|$PATH|g" \
        -e "s|__LOG__|$LOG|g" \
        "$src" > "$dest"

    # A token that survived substitution means a template gained a placeholder
    # that render() was never taught about. Failing here is the difference
    # between a loud install error and a knob that silently does nothing.
    if grep -q '__[A-Z_]*__' "$dest"; then
        echo "ERROR: unrendered tokens in $dest:" >&2
        grep -o '__[A-Z_]*__' "$dest" | sort -u | sed 's/^/  /' >&2
        exit 1
    fi
}

if [ "$PLATFORM" = "Darwin" ]; then
    UID_NUM="$(id -u)"
    AGENT_DIR="$HOME/Library/LaunchAgents"
    DEST="$AGENT_DIR/$LABEL.plist"
    LOG_DIR="$HOME/Library/Logs/dabba"
    LOG="$LOG_DIR/reconcile-$DABBA_ENVIRONMENT.log"
    mkdir -p "$AGENT_DIR" "$LOG_DIR"

    render "$BACKEND_DIR/launchd/io.spicelabs.dabba.reconcile.plist.template" "$DEST"

    # Modern launchctl (macOS 11+): bootstrap into the per-user GUI domain.
    # (Legacy equivalent, if you ever need it on an old OS:
    #    launchctl load -w "$DEST"  /  launchctl unload -w "$DEST" )
    launchctl bootout "gui/$UID_NUM/$LABEL" > /dev/null 2>&1 || true
    launchctl bootstrap "gui/$UID_NUM" "$DEST"
    launchctl enable "gui/$UID_NUM/$LABEL"
    launchctl kickstart "gui/$UID_NUM/$LABEL"   # run one pass now (RunAtLoad also fires)

    echo "installed LaunchAgent $LABEL"
    echo "  plist : $DEST"
    echo "  log   : $LOG"
    echo "  config: GITOPS_DIR=$GITOPS_DIR STACKS_DIR=$STACKS_DIR BOX_NAME=$BOX_NAME"
    echo "  status: launchctl print gui/$UID_NUM/$LABEL"
else
    LOG=""   # systemd captures to the journal, not a file
    UNIT_DIR="$HOME/.config/systemd/user"
    mkdir -p "$UNIT_DIR"

    render "$BACKEND_DIR/systemd/gitops-reconcile.service.template" "$UNIT_DIR/$UNIT_BASE.service"
    render "$BACKEND_DIR/systemd/gitops-reconcile-alert.service.template" "$UNIT_DIR/$UNIT_BASE-alert.service"
    render "$BACKEND_DIR/systemd/gitops-reconcile.timer.template" "$UNIT_DIR/$UNIT_BASE.timer"
    # A job unit from gitops content activates this with
    # OnFailure=__JOB_ALERT__%n.service, which the reconciler renders.
    render "$BACKEND_DIR/systemd/dabba-job-alert@.service.template" "$UNIT_DIR/$JOB_ALERT.service"

    systemctl --user daemon-reload
    systemctl --user enable --now "$UNIT_BASE.timer"

    # So the timer runs without an active login session. Needs privileges; do
    # not fail the install if it cannot be set here.
    if ! loginctl enable-linger "$USER" > /dev/null 2>&1; then
        echo "NOTE: run 'sudo loginctl enable-linger $USER' so the timer runs"
        echo "      without you being logged in."
    fi

    echo "installed systemd user timer $UNIT_BASE.timer"
    echo "  units : $UNIT_DIR/$UNIT_BASE.{service,timer}"
    echo "  config: GITOPS_DIR=$GITOPS_DIR STACKS_DIR=$STACKS_DIR BOX_NAME=$BOX_NAME"
    echo "  status: systemctl --user status $UNIT_BASE.timer"
    echo "  logs  : journalctl --user -u $UNIT_BASE.service -f"
fi
