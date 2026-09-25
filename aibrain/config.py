"""Configuration for the AI Brain kernel.

The config lives in a single JSON file so it can be edited by hand or through
the UI. It is created on first run by discovering Obsidian vaults in the usual
places, so a fresh checkout starts with something to look at.
"""

from __future__ import annotations

import json
import os
import uuid
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CONFIG_DIR = Path(
    os.environ.get("AIBRAIN_HOME", Path.home() / ".aibrain")
).expanduser()

# Palette lifted from the design concept: one hue per source ribbon.
SOURCE_COLORS = [
    "#4db3f0",  # blue
    "#f06aa6",  # pink
    "#f0c030",  # amber
    "#3ecf9a",  # green
    "#9b7cf0",  # violet
    "#f2952d",  # orange
    "#4fd6d0",  # teal
    "#e0637a",  # rose
    "#7fd8e8",  # cyan
    "#c0d05a",  # lime
]

AGENT_COLORS = ["#b48cff", "#ffb347", "#ff7a59", "#6ee7b7", "#7fd8e8"]

# Agents with no saved position are placed by the graph builder, which knows
# how big the galaxies turned out; dragging one in the UI pins it here.


def _new_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


@dataclass
class BrainConfig:
    """One Obsidian vault rendered as one galaxy."""

    id: str
    name: str
    path: str
    enabled: bool = True
    # Grid slot is resolved at build time; an explicit center pins it.
    center: list[float] | None = None
    radius: float | None = None
    seed: int | None = None
    # "" means unset: reconcile_brains() backfills it from SOURCE_COLORS the
    # first time this brain is seen, same as an agent's color is picked once
    # at creation rather than recomputed on every read.
    color: str = ""
    # Which folder a note's ribbon and dot color follow: "top_folder" puts
    # Notes/2025/today.md on "Notes", "folder" puts it on "Notes/2025".
    # Read by the Rust layout; changing it needs no reindex.
    group_by: str = "top_folder"
    exclude: list[str] = field(
        default_factory=lambda: [".obsidian", ".trash", ".git", "ZZ-Attachments",
                                 "ZZ-Attachements", "assets", "scans", "Excalidraw"]
    )
    # At most one brain may be the meeting-recording target at a time —
    # enforced in server.py's save_brain, which clears it on the others.
    # A new brain always starts with this off.
    meeting_target: bool = False
    # Where raw_transcripts/ gets absorbed into, relative to this vault's root.
    meeting_folder: str = "Meetings"

    def resolved_path(self) -> Path:
        return Path(self.path).expanduser()

    def resolved_meeting_folder(self) -> Path:
        return self.resolved_path() / (self.meeting_folder or "Meetings")


@dataclass
class AgentConfig:
    """A conversational endpoint shown as a star in the universe.

    kind:
      local  -- built-in retrieval agent, answers from the index itself
      acp    -- Agent Client Protocol over stdio (Claude Code, Codex, ...)
      a2a    -- Agent2Agent over HTTP (Elasticsearch Agent Builder, ...)
    """

    id: str
    name: str
    kind: str = "local"
    color: str = "#b48cff"
    protocol: str = ""
    intro: str = ""
    suggestions: list[str] = field(default_factory=list)
    pos: list[float] | None = None
    enabled: bool = True

    # acp
    command: list[str] = field(default_factory=list)
    cwd: str = ""
    env: dict[str, str] = field(default_factory=dict)

    # a2a
    url: str = ""
    headers: dict[str, str] = field(default_factory=dict)

    # Whether we pre-fetch passages and stuff them into the prompt, versus
    # sending the clean question and letting the agent search on its own
    # (an MCP tool for acp, its own retrieval for a2a). Off by default: an
    # a2a agent like Elastic Agent Builder runs its own index, and an acp
    # agent like Claude Code or Codex can call `search_notes` itself when it
    # decides the question needs it, which beats us guessing on every turn.
    ground_with_context: bool = False
    context_notes: int = 6
    context_chars: int = 1200
    brains: list[str] = field(default_factory=list)  # empty == all; also scopes the acp search tool

    def label(self) -> str:
        if self.protocol:
            return self.protocol
        return {"local": "LOCAL", "acp": "ACP", "a2a": "A2A"}.get(self.kind, self.kind.upper())


@dataclass
class ScriptConfig:
    """A maintenance script the UI can run and stream output from."""

    id: str
    name: str
    description: str = ""
    command: list[str] = field(default_factory=list)
    cwd: str = ""
    args_hint: str = ""
    # Optional named toggles, e.g. {"Dry run": ["--dry-run"]}
    options: dict[str, list[str]] = field(default_factory=dict)
    reindex_after: bool = False


@dataclass
class ViewOptions:
    rotation_speed: float = 0.35
    link_opacity: float = 0.24
    show_all_labels: bool = False
    ribbon_twist: float = 0.25


@dataclass
class TodoOptions:
    """Settings for the to-do list. Python writes them; Rust reads them.

    04:00 rather than midnight so that at one in the morning "today" and
    "tomorrow" still mean the day you were working and the one after it.
    """

    day_start_hour: int = 4


@dataclass
class Config:
    brains: list[BrainConfig] = field(default_factory=list)
    agents: list[AgentConfig] = field(default_factory=list)
    scripts: list[ScriptConfig] = field(default_factory=list)
    view: ViewOptions = field(default_factory=ViewOptions)
    todo: TodoOptions = field(default_factory=TodoOptions)
    host: str = "127.0.0.1"
    port: int = 8760
    title: str = "Dave Brain"

    path: Path = field(default=DEFAULT_CONFIG_DIR / "config.json", repr=False)

    # ---- persistence -----------------------------------------------------
    @classmethod
    def load(cls, path: Path | None = None) -> "Config":
        path = Path(path or DEFAULT_CONFIG_DIR / "config.json").expanduser()
        if not path.exists():
            cfg = default_config()
            cfg.path = path
            cfg.save()
            return cfg
        raw = json.loads(path.read_text(encoding="utf-8"))
        cfg = cls.from_dict(raw)
        cfg.path = path
        # The symlink folder is the source of truth for which vaults exist, so
        # a link added or removed on disk takes effect on the next start
        # without anyone editing config.json.
        if reconcile_brains(cfg):
            cfg.save()
        return cfg

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> "Config":
        def build(kls, items):
            fields = {f for f in kls.__dataclass_fields__}
            return [kls(**{k: v for k, v in it.items() if k in fields}) for it in items]

        view_raw = raw.get("view", {}) or {}
        view = ViewOptions(
            **{k: v for k, v in view_raw.items() if k in ViewOptions.__dataclass_fields__}
        )
        todo_raw = raw.get("todo", {}) or {}
        todo = TodoOptions(
            **{k: v for k, v in todo_raw.items() if k in TodoOptions.__dataclass_fields__}
        )
        if not 0 <= todo.day_start_hour <= 23:
            todo.day_start_hour = TodoOptions().day_start_hour
        return cls(
            brains=build(BrainConfig, raw.get("brains", [])),
            agents=build(AgentConfig, raw.get("agents", [])),
            scripts=build(ScriptConfig, raw.get("scripts", [])),
            view=view,
            todo=todo,
            host=raw.get("host", "127.0.0.1"),
            port=int(raw.get("port", 8760)),
            title=raw.get("title", "Dave Brain"),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "title": self.title,
            "host": self.host,
            "port": self.port,
            "view": asdict(self.view),
            "todo": asdict(self.todo),
            "brains": [asdict(b) for b in self.brains],
            "agents": [asdict(a) for a in self.agents],
            "scripts": [asdict(s) for s in self.scripts],
        }

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self.to_dict(), indent=2), encoding="utf-8")
        tmp.replace(self.path)

    # ---- lookups ---------------------------------------------------------
    def brain(self, bid: str) -> BrainConfig | None:
        return next((b for b in self.brains if b.id == bid), None)

    def agent(self, aid: str) -> AgentConfig | None:
        return next((a for a in self.agents if a.id == aid), None)

    def script(self, sid: str) -> ScriptConfig | None:
        return next((s for s in self.scripts if s.id == sid), None)

    def enabled_brains(self) -> list[BrainConfig]:
        return [b for b in self.brains if b.enabled and b.resolved_path().is_dir()]

    def enabled_agents(self) -> list[AgentConfig]:
        return [a for a in self.agents if a.enabled]


# ---------------------------------------------------------------------------
# which vaults exist
# ---------------------------------------------------------------------------

# The symlink folder is the allowlist, not a hint. A vault is visible to this
# app if, and only if, it is linked from here — so adding one is `ln -s`, and
# removing one is `rm`, with no way for a scan to volunteer a vault you did not
# ask for.
VAULT_LINK_DIR = REPO_ROOT / "obsidian_vaults"


def discover_vaults() -> list[Path]:
    """Vaults linked from `obsidian_vaults/`, de-duplicated by real path.

    Broken links are skipped rather than reported, because the caller only ever
    wants the ones it can actually read; `link_problems()` explains the rest.
    """
    found: dict[Path, Path] = {}
    base = VAULT_LINK_DIR
    if not base.is_dir():
        return []
    for child in sorted(base.iterdir()):
        try:
            if child.name.startswith(".") or not child.is_dir():
                continue
            real = child.resolve()
            if real in found:
                continue
            found[real] = child
        except (OSError, PermissionError):
            continue
    return list(found.values())


def link_problems() -> list[tuple[str, str]]:
    """Entries in `obsidian_vaults/` that do not resolve, as (name, reason).

    A dangling symlink is silent otherwise — the vault simply never appears —
    so the UI surfaces these instead of leaving you to wonder.
    """
    out: list[tuple[str, str]] = []
    if not VAULT_LINK_DIR.is_dir():
        return out
    for child in sorted(VAULT_LINK_DIR.iterdir()):
        if child.name.startswith("."):
            continue
        if child.is_dir():
            continue
        if child.is_symlink():
            out.append((child.name, f"link points at {os.readlink(child)}, "
                                    f"which does not exist"))
        else:
            out.append((child.name, "not a directory"))
    return out


def reconcile_brains(cfg: "Config") -> bool:
    """Make the brain list match the symlinks, keeping per-brain settings.

    Settings are merged back by id, so unlinking a vault and relinking it later
    restores how it was configured rather than resetting it. Returns whether
    anything changed, so the caller knows to save.
    """
    linked = discover_vaults()
    by_id: dict[str, BrainConfig] = {}
    backfilled = False
    for i, path in enumerate(linked):
        bid = slugify(path.name)
        prior = cfg.brain(bid)
        if prior is not None:
            prior.path = str(path)
            if not prior.color:
                prior.color = SOURCE_COLORS[i % len(SOURCE_COLORS)]
                backfilled = True
            by_id[bid] = prior
        else:
            by_id[bid] = BrainConfig(
                id=bid, name=path.name, path=str(path), seed=7 + i * 13,
                color=SOURCE_COLORS[i % len(SOURCE_COLORS)])

    changed = backfilled or [b.id for b in cfg.brains] != list(by_id)
    if not changed:
        changed = any(cfg.brain(b).path != by_id[b].path for b in by_id)
    cfg.brains = list(by_id.values())
    write_deny_rules(linked)
    return changed


# Agents run with the repo as their working directory, so the vaults are only
# out of reach because this file says so. A static list stops protecting a
# vault the moment one is linked, and keeps denying one that was unlinked, so
# it is regenerated from the symlinks every time they are reconciled.
#
# settings.local.json, not settings.json: the rules bake in this machine's
# absolute vault paths (obsidian_vaults/ is itself local and gitignored), so
# they belong in Claude Code's untracked local-overrides file. Writing them
# into the committed settings.json would make every checkout's commits fight
# over each other's paths.
SETTINGS_PATH = REPO_ROOT / ".claude" / "settings.local.json"

# Not vaults, and nothing to do with which brains exist — these stay whatever
# the vault list does.
FIXED_DENY = [
    "~/.ssh", "~/.aws", "~/.gnupg", "~/.config/gh", "~/Library/Keychains",
]


def _read_rule(path: Path) -> str:
    return f"Read({path}/**)"


def deny_rules(vaults: list[Path]) -> list[str]:
    """The deny list a given set of linked vaults implies.

    Symlinks are resolved because the rule has to name the real directory: a
    deny on the link is not a deny on what it points at.
    """
    rules = []
    for vault in vaults:
        try:
            real = vault.resolve()
        except OSError:
            continue
        rules.append(_read_rule(real))
    for entry in FIXED_DENY:
        rules.append(_read_rule(Path(entry).expanduser()))
    # Stable order, no duplicates, so a no-op reconcile does not rewrite it.
    seen, out = set(), []
    for rule in rules:
        if rule not in seen:
            seen.add(rule)
            out.append(rule)
    return out


def write_deny_rules(vaults: list[Path]) -> bool:
    """Add the Read denials to `.claude/settings.local.json`. Returns if changed.

    Everything else in the file is left alone, and so is every rule that is
    already there: this only ever adds. See the note where `wanted` is built
    for why a rule is never taken away.
    """
    path = SETTINGS_PATH
    # Loading the config is what triggers this, and plenty of things load the
    # config without meaning to touch a machine-local file that stays out of
    # git — a test, a script, an editor plugin. The escape hatch is an env
    # var so those can say so without the app losing the behaviour it wants.
    if os.environ.get("AIBRAIN_MANAGE_DENY_RULES", "1") in ("0", "no", "false"):
        return False
    if not path.parent.is_dir():
        return False
    # An empty `obsidian_vaults/` means the user unlinked everything, and the
    # rules should go with them. A *missing* one means this checkout has never
    # had links at all — a fresh clone, or a worktree — and we know nothing
    # about which vaults exist, so rewriting the local settings file to drop
    # every vault rule would take protection away rather than keep it current.
    if not VAULT_LINK_DIR.is_dir():
        return False
    try:
        raw = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    except (OSError, json.JSONDecodeError):
        # A settings file we cannot parse is not ours to overwrite.
        return False
    if not isinstance(raw, dict):
        return False

    permissions = raw.setdefault("permissions", {})
    if not isinstance(permissions, dict):
        return False
    prior = permissions.get("deny")
    prior = prior if isinstance(prior, list) else []
    # Paths that the user has explicitly allowed should not be denied — deny
    # wins over allow in Claude Code, so a generated deny rule would silently
    # override a hand-written allow rule and block access the user intended.
    # Extract the bare path prefixes from every allow rule so we can do a
    # simple prefix check rather than an exact string match.
    raw_allowed = permissions.get("allow")
    raw_allowed = raw_allowed if isinstance(raw_allowed, list) else []
    allowed_prefixes = []
    for rule in raw_allowed:
        # Allow rules look like Read(/some/path/**) or Read(/some/path/*)
        if rule.startswith("Read(") and rule.endswith(")"):
            p = rule[5:-1].rstrip("*").rstrip("/")
            allowed_prefixes.append(p)

    def _is_allowed(deny_rule: str) -> bool:
        if not (deny_rule.startswith("Read(") and deny_rule.endswith(")")):
            return False
        p = deny_rule[5:-1].rstrip("*").rstrip("/")
        return any(p == a or p.startswith(a + "/") or a.startswith(p + "/")
                   for a in allowed_prefixes)

    # Only ever adds. The earlier version rebuilt the whole `Read(` block from
    # the current symlinks, which meant an unlinked vault lost its rule — and
    # so did any `Read(` denial someone had written by hand, because nothing in
    # the file says which rules are ours. A rule that outlives its vault costs
    # nothing; a rule that vanishes costs the protection it was there for. So
    # a vault is unprotected only after the user edits this file themselves.
    generated = [r for r in deny_rules(vaults) if not _is_allowed(r)]
    wanted = generated + [r for r in prior if r not in generated]
    if wanted == prior:
        return False

    permissions["deny"] = wanted
    tmp = path.with_suffix(".json.tmp")
    try:
        tmp.write_text(json.dumps(raw, indent=2) + "\n", encoding="utf-8")
        tmp.replace(path)
    except OSError:
        tmp.unlink(missing_ok=True)
        return False
    return True


def slugify(name: str) -> str:
    out = "".join(c.lower() if c.isalnum() else "-" for c in name).strip("-")
    while "--" in out:
        out = out.replace("--", "-")
    return out or "brain"


def _on_path(name: str) -> bool:
    import shutil
    return shutil.which(name) is not None


def default_agents() -> list[AgentConfig]:
    # An adapter that is already installed is almost certainly meant to be used,
    # so turn those on; the rest stay listed but dark until they are configured.
    return [
        AgentConfig(
            id="kernel",
            name="Kernel",
            kind="local",
            color="#b48cff",
            protocol="LOCAL RETRIEVAL",
            intro=(
                "Kernel reads straight from the index. Ask it anything and it will "
                "pull the notes that answer it, ranked, with the passage that matched."
            ),
            suggestions=[
                "What do I know about agent protocols?",
                "Find every note mentioning Claude Code",
                "Which notes link to my journal the most?",
            ],
        ),
        AgentConfig(
            id="claude-code",
            name="Claude Code",
            kind="acp",
            color="#ff7a59",
            protocol="ACP",
            intro=(
                "Connected over the Agent Client Protocol. Reads the markdown behind "
                "your brains and can edit or link the files directly."
            ),
            suggestions=[
                "Summarize what changed in my vault this week",
                "Which notes are orphaned?",
                "Draft a note linking my open projects",
            ],
            command=["claude-agent-acp"],
            # A dedicated cwd, not REPO_ROOT: the SDK resolves project
            # settings/skills/CLAUDE.md from this session's cwd
            # (@agentclientprotocol/claude-agent-acp watches
            # <cwd>/.claude/settings*.json directly), so pointing it at
            # acp-claude/ keeps the kernel dev config — hooks, permissions,
            # skills meant for editing this repo — out of the vault-Q&A agent,
            # and vice versa.
            cwd=str(REPO_ROOT / "acp-claude"),
            enabled=_on_path("claude-agent-acp"),
        ),
        AgentConfig(
            id="codex",
            name="Codex",
            kind="acp",
            color="#4fd6d0",
            protocol="ACP",
            intro=(
                "Connected over the Agent Client Protocol via the Codex CLI. Reads "
                "the same retrieved passages and the files behind them."
            ),
            suggestions=[
                "Explain how the exporter script works",
                "Where does the index get built?",
                "What are the oldest notes in my vault about?",
            ],
            command=["codex-acp"],
            # A dedicated cwd, not REPO_ROOT: codex-acp spawns the real `codex
            # app-server` binary, which resolves AGENTS.md, .agents/skills/,
            # and project .codex/config.toml layers from this session's cwd —
            # same separation as the Claude Code agent, above, just via
            # Codex's own conventions instead of Claude Code's.
            cwd=str(REPO_ROOT / "acp-codex"),
            enabled=_on_path("codex-acp"),
        ),
        AgentConfig(
            id="elastic",
            name="Elasticsearch Agent Builder",
            kind="a2a",
            color="#ffb347",
            protocol="A2A",
            intro=(
                "Connected over Agent2Agent. Runs hybrid search over the indexed "
                "notes and returns ranked passages."
            ),
            suggestions=[
                "Search meetings about pricing",
                "Show notes I saved last month",
            ],
            url="http://localhost:9000",
            enabled=False,
        ),
    ]


def default_scripts() -> list[ScriptConfig]:
    return [
        ScriptConfig(
            id="macwhisper",
            name="MacWhisper transcripts",
            description=(
                "Tail MacWhisper's database and mirror new transcripts into "
                "raw_transcripts/, a staging folder for absorbing into a vault. "
                "Quits and relaunches MacWhisper."
            ),
            command=["python3", str(REPO_ROOT / "scripts/macwhisper/macwhisper_export.py")],
            cwd=str(REPO_ROOT),
            options={"Dry run": ["--dry-run"], "Verbose": ["--verbose"]},
            # Writes to the raw_transcripts/ staging folder, not a linked vault,
            # so there is nothing new for the corpus to index yet.
            reindex_after=False,
        ),
    ]


def default_config() -> Config:
    cfg = Config(agents=default_agents(), scripts=default_scripts())
    reconcile_brains(cfg)
    return cfg
