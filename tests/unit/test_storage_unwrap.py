#
# Copyright (c) 2019 Matthias Tafelmeier.
#
# AGPL-3.0 — see godon-systemtenders/LICENSE.
#
"""Storage-unwrap tests — the wish pulse reads through optuna's wrapper.

Live receipt (run 35454144639, wish 2befb29d): the assignment check
died every trial boundary with "_CachedStorage object has no attribute
'engine'" — optuna wraps RDBStorage in a caching layer that delegates
everything EXCEPT the raw engine, and the pulse's SQL needs the engine
(same archive DB as the shutdown flag). The unit tests mocked a bare
storage with .engine, so the wrapper never appeared in vitro. These
tests pin the unwrap: outer wrapper without .engine, inner backend
with it.
"""

import sys, os, types
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

import pytest


class _Inner:
    """The RDBStorage stand-in: has the engine."""

    class engine:
        @staticmethod
        def connect():
            raise RuntimeError("not exercised by these tests")


class _CachedWrapper:
    """Optuna's _CachedStorage stand-in: delegates, hides the engine."""

    def __init__(self, backend):
        self._backend = backend

    def get_all_trials(self):
        return self._backend.get_all_trials()


def test_unwrap_reaches_the_engine():
    from engine.systemtender_worker import SystemtenderWorker
    wrapped = _CachedWrapper(_Inner())
    engine = SystemtenderWorker._unwrap_storage_engine(
        types.SimpleNamespace(_storage=wrapped))
    assert engine is _Inner.engine


def test_bare_storage_passes_through():
    from engine.systemtender_worker import SystemtenderWorker
    bare = _Inner()
    engine = SystemtenderWorker._unwrap_storage_engine(
        types.SimpleNamespace(_storage=bare))
    assert engine is bare.engine


def test_unknown_wrapper_raises_loudly():
    from engine.systemtender_worker import SystemtenderWorker
    opaque = types.SimpleNamespace()  # no engine, no _backend
    with pytest.raises(RuntimeError):
        SystemtenderWorker._unwrap_storage_engine(
            types.SimpleNamespace(_storage=opaque))
