import sys
from sys import exit
from pathlib import Path
from importlib import import_module
from json import loads
from types import ModuleType

################################################################
# Begin janky attempt to workaround
#   https://github.com/pypa/setuptools/issues/3786
################################################################
import sysconfig
import distutils.sysconfig


def get_python_inc(plat_specific=0, prefix=None):
    if plat_specific:
        return sysconfig.get_path("platinclude")
    else:
        return sysconfig.get_path("include")


distutils.sysconfig.get_python_inc = get_python_inc
################################################################
# End janky workaround
################################################################

def get_backend_from_entrypoint(entrypoint: str) -> ModuleType:
    # https://packaging.python.org/en/latest/specifications/entry-points/
    modname, qualname_separator, qualname = entrypoint.partition(":")
    backend = import_module(modname)
    if qualname_separator:
        for attr in qualname.split("."):
            backend = getattr(backend, attr)
            if backend is None:
                raise AttributeError(f"Attribute '{attr}' not found in '{modname}'")

    return backend

def get_backend_paths(backend_paths: [str]):
    result = []
    cwd = Path.cwd().absolute()
    for path in backend_paths:
        resolved = Path(path).resolve()
        if not resolved.is_relative_to(cwd):
            print(f"Invalid pyproject.toml build-system.backend-path {resolved}")
            exit(1)
        result.append(str(resolved))
    return result

def get_requires_for_build_wheel(backend: ModuleType) -> [str]:
    """
    Return an list of requirements. This is only necessary if we do not
    have a pyproject.toml file.
    """
    f = getattr(backend, "get_requires_for_build_wheel")
    if f is None:
        return []
    return f()

def prepare_metadata_for_build_wheel(backend: ModuleType, work_dir: Path):
    """
    Prepare any files that need to be generated before building the wheel.
    """
    metadata_dir = work_dir / "prepare_metadata_for_build_wheel"

    if hasattr(backend, "prepare_metadata_for_build_wheel"):
        metadata_dir.mkdir()
        dist_info = backend.prepare_metadata_for_build_wheel(str(metadata_dir))
        (work_dir / "prepare_metadata_for_build_wheel.out").write_text(dist_info, "utf-8")
        exit(0)
    else:
        exit(123)

def build_wheel(backend: ModuleType, work_dir: Path):
    """Take a folder with an SDist and build a wheel from it."""

    wheel_dir = work_dir / "build_wheel"
    metadata_dir = work_dir / "prepare_metadata_for_build_wheel"

    wheel_dir.mkdir()
    wheel_basename = backend.build_wheel(
        str(wheel_dir),
        metadata_directory=str(metadata_dir) if metadata_dir.exists() else None,
    )

    (work_dir / "build_wheel.out").write_text(wheel_basename, "utf-8")

if __name__ == "__main__":
    work_dir, goal = sys.argv[1:]
    backend = get_backend_from_entrypoint(sys.argv[3])
    build_system = loads((work_dir / "build-system.json").read_text("utf-8"))
    sys.path[:0] = get_backend_paths(build_system["backend_path"])

    if goal == "GetRequiresForBuildWheel":
        requires = get_requires_for_build_wheel(backend)
    if goal == "WheelMetadata":
        prepare_metadata_for_build_wheel()
    elif goal == "BuildWheel":
        build_wheel()

    exit(0)