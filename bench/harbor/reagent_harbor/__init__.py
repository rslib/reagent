"""A Harbor installed-agent adapter that runs the reagent binary.

The adapter's ``ReagentAgent`` copies the statically linked ``dist/reagent``
binary into the task container, runs it in ``--json`` mode, and reports the
token and turn counts from the event stream back to Harbor.
"""

from reagent_harbor.agent import ReagentAgent

__all__ = ["ReagentAgent"]