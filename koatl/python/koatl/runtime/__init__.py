"""
This module initializes the environment for koatl.
It sets up the meta-finder hook to enable importing .tl files,
and also declares functions that are required for certain tl features to work,
such as coalescing and module exports.
"""

from . import meta_finder

meta_finder.install_hook()
del meta_finder

from .._rs import Record


def _set_exports(package_name, globals_dict, exports, module_star_exports):
    import importlib

    exports = set(exports)

    for module in module_star_exports:
        mod = importlib.import_module(module, package_name)

        if hasattr(mod, "__all__"):
            for name in mod.__all__:
                exports.add(name)
        else:
            for name in dir(mod):
                if name.startswith("_"):
                    continue

                exports.add(name)

    globals_dict["__all__"] = tuple(exports)


def _coalesces(x):
    return x is None or isinstance(x, BaseException)


def _match_proxy(v):
    from types import SimpleNamespace

    return SimpleNamespace(value=v)


class MatchError(Exception):
    def __init__(self, message):
        super().__init__(message)


def _slice_iter(sl):
    i = 0 if sl.start is None else sl.start
    step = 1 if sl.step is None else sl.step

    if sl.stop is None:
        while True:
            yield i
            i += step
    else:
        yield from range(i, sl.stop, step)


from collections import defaultdict

_vtable = defaultdict(dict, {})
del defaultdict


# TODO move this to rust?
def _vget(obj, name):
    if name == "iter":
        if isinstance(obj, slice):
            return _slice_iter(obj)
        if hasattr(obj, "items"):
            return obj.items
        if hasattr(obj, "__iter__"):
            return obj.__iter__
        raise TypeError(f"'{type(obj).__name__}' object is not iterable")

    raise AttributeError(f"'{type(obj).__name__}' object has no attribute '{name}'")


def iter(obj):
    return _vget(obj, "iter")()


__all__ = [
    "Record",
    "_coalesces",
    "_set_exports",
    "_match_proxy",
    "MatchError",
    "iter",
    "_vget",
    "_vtable",
]
