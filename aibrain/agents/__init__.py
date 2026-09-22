"""Agent registry.

Connections are built lazily and kept, because an ACP subprocess is expensive
to start and should survive between questions.
"""

from __future__ import annotations

import threading

from ..config import AgentConfig, Config
from ..corpus import Corpus
from .a2a import A2AAgent
from .acp import ACPAgent
from .base import Agent, Citation, Event
from .local import LocalAgent

KINDS = {"local": LocalAgent, "acp": ACPAgent, "a2a": A2AAgent}


class Registry:
    def __init__(self, cfg: Config, corpus: Corpus):
        self.cfg = cfg
        self.corpus = corpus
        self._agents: dict[str, Agent] = {}
        self._lock = threading.Lock()

    def _brain_names(self) -> dict[str, str]:
        return {b.id: b.name for b in self.cfg.brains}

    def _colors(self) -> dict[str, str]:
        from ..config import SOURCE_COLORS
        return {b.id: SOURCE_COLORS[i % len(SOURCE_COLORS)]
                for i, b in enumerate(self.cfg.brains)}

    def get(self, agent_id: str) -> Agent | None:
        cfg = self.cfg.agent(agent_id)
        if cfg is None:
            return None
        with self._lock:
            existing = self._agents.get(agent_id)
            if existing is not None and existing.cfg == cfg:
                return existing
            if existing is not None:
                existing.close()
            kls = KINDS.get(cfg.kind, LocalAgent)
            agent = kls(cfg, self.corpus, self._brain_names(), self._colors())
            # An ACP agent gets file access to the vaults as well as its own
            # working directory, so it can open the notes it is asked about.
            if hasattr(agent, "vault_roots"):
                brains = self.cfg.enabled_brains()
                agent.vault_roots = [b.resolved_path() for b in brains]
                # Keyed by brain so a path the agent opened maps back to a note.
                agent.brain_roots = {b.id: str(b.resolved_path()) for b in brains}
            self._agents[agent_id] = agent
            return agent

    def invalidate(self, agent_id: str | None = None) -> None:
        """Drop cached connections after a config change."""
        with self._lock:
            targets = [agent_id] if agent_id else list(self._agents)
            for key in targets:
                agent = self._agents.pop(key, None)
                if agent:
                    agent.close()

    def close(self) -> None:
        self.invalidate()


__all__ = ["Agent", "Citation", "Event", "Registry", "KINDS",
           "LocalAgent", "ACPAgent", "A2AAgent", "AgentConfig"]
