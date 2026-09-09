"""
Test bootstrap — stubs the Windmill-runtime imports that don't exist
in the local test environment.
"""
import sys
import types
import logging

# Stub f.systemtender.shared.otel_logging
f_mod = types.ModuleType('f')
systemtender_mod = types.ModuleType('f.systemtender')
shared_mod = types.ModuleType('f.systemtender.shared')
otel_mod = types.ModuleType('f.systemtender.shared.otel_logging')
engine_mod = types.ModuleType('f.systemtender.engine')

def get_logger(name):
    return logging.getLogger(name)

otel_mod.get_logger = get_logger

f_mod.systemtender = systemtender_mod
systemtender_mod.shared = shared_mod
shared_mod.otel_logging = otel_mod
systemtender_mod.engine = engine_mod

sys.modules['f'] = f_mod
sys.modules['f.systemtender'] = systemtender_mod
sys.modules['f.systemtender.shared'] = shared_mod
sys.modules['f.systemtender.shared.otel_logging'] = otel_mod
sys.modules['f.systemtender.engine'] = engine_mod
