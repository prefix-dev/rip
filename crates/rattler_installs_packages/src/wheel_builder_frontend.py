import sys
from sys import exit
from pathlib import Path
from importlib import import_module
from json import loads
from types import ModuleType
import json

################################################################
# Begin janky attempt to workaround
#   https://github.com/pypa/setuptools/issues/3786
################################################################

try:
    import sysconfig
    import distutils.sysconfig


    def get_python_inc(plat_specific=0, prefix=None):
        if plat_specific:
            return sysconfig.get_path("platinclude")
        else:
            return sysconfig.get_path("include")


    distutils.sysconfig.get_python_inc = get_python_inc
except ImportError:
    # we ignore missing distutils (was removed in Python 3.12)
    pass

################################################################
# End janky workaround
################################################################

def get_backend_from_entry_point(entrypoint: str) -> ModuleType:
    # https://packaging.python.org/en/latest/specifications/entry-points/
    modname, qualname_separator, qualname = entrypoint.partition(":")
    backend = import_module(modname)
    if qualname_separator:
        for attr in qualname.split("."):
            backend = getattr(backend, attr)
            if backend is None:
                raise AttributeError(f"Attribute '{attr}' not found in '{modname}'")

    return backend

def get_requires_for_build_wheel(backend: ModuleType, work_dir: Path) -> [str]:
    """
    Returns a list of requirements. This is only necessary if we do not
    have a pyproject.toml file.
    """
    f = getattr(backend, "get_requires_for_build_wheel")
    if f is None:
        result = []
    else:
        result = f()

    j = json.dumps(result)
    out_json_file = work_dir / "extra_requirements.json"
    out_json_file.write_text(j)
    print(j)

def metadata_dirs(work_dir: Path):
    return work_dir / "metadata"

def prepare_metadata_for_build_wheel(backend: ModuleType, work_dir: Path):
    """
    Prepare any files that need to be generated before building the wheel.
    """
    if hasattr(backend, "prepare_metadata_for_build_wheel"):
        # Create an output file for the metadata
        result_file = work_dir / "metadata_result"

        # Create the metadata output directory
        d = metadata_dirs(work_dir)
        d.mkdir()
        dist_info = backend.prepare_metadata_for_build_wheel(str(d))
        # Path to the dist-info directory
        result = str(d / dist_info)
        # Write the path to the dist-info directory to a file
        result_file.write_text(result)
    else:
        exit(123)

def wheel_dirs(work_dir: Path):
    return work_dir / "wheel"

def build_wheel(backend: ModuleType, work_dir: Path):
    """Take a folder with an SDist and build a wheel from it."""
    wheel_dir = wheel_dirs(work_dir)

    # TODO: maybe start reading this from the file again if it fails
    metadata_dir = metadata_dirs(work_dir) / ".dist-info"
    # Use the metadata directory if it exists, otherwise set this to None

    wheel_dir.mkdir()
    wheel_basename = backend.build_wheel(
        str(wheel_dir),
        metadata_directory=metadata_dir if metadata_dir.exists() else None,
    )

    print(str(wheel_dir / wheel_basename))

if __name__ == "__main__":
    work_dir, entry_point, goal = sys.argv[1:]
    backend = get_backend_from_entry_point(entry_point)

    work_dir = Path(work_dir)

    if goal == "GetRequiresForBuildWheel":
        get_requires_for_build_wheel(backend, work_dir)
    if goal == "WheelMetadata":
        prepare_metadata_for_build_wheel(backend, work_dir)
    elif goal == "Wheel":
        build_wheel(backend, work_dir)

    exit(0)
