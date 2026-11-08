from hatchling.plugin import hookimpl

from .plugin import AnubisBuildHook


@hookimpl
def hatch_register_build_hook():
    return AnubisBuildHook
