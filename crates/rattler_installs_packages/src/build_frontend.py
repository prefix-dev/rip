import sys

# Import names we'll need out of an abundance of caution, in case the backend ends up
# monkeypatching the original module or something.
from pathlib import Path
from importlib import import_module
from sys import exit
from json import loads, dumps

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


(work_dir, goal, binary_wheel_tag) = sys.argv[1:]

work_dir = Path(work_dir)
build_system = loads((work_dir / "build-system.json").read_text("utf-8"))

backend_paths = []
cwd = Path.cwd().absolute()
for backend_path in build_system["backend_path"]:
    resolved = Path(backend_path).resolve()
    if not resolved.is_relative_to(cwd):
        print(f"Invalid pyproject.toml build-system.backend-path {resolved}")
        exit(1)
    backend_paths.append(str(resolved))
sys.path[:0] = backend_paths

# https://packaging.python.org/en/latest/specifications/entry-points/
modname, qualname_separator, qualname = build_system["build_backend"].partition(":")
backend = import_module(modname)
if qualname_separator:
    for attr in qualname.split("."):
        backend = getattr(backend, attr)

if not (work_dir / "get_requires_for_build_wheel").exists():
    try:
        f = backend.get_requires_for_build_wheel
    except AttributeError:
        requires = []
    else:
        requires = f()
    (work_dir / "get_requires_for_build_wheel").write_text(dumps(requires), "utf-8")
    if requires:
        exit(0)

metadata_dir = work_dir / "prepare_metadata_for_build_wheel"

if goal == "WheelMetadata" and hasattr(backend, "prepare_metadata_for_build_wheel"):
    metadata_dir.mkdir()
    dist_info = backend.prepare_metadata_for_build_wheel(str(metadata_dir))
    (work_dir / "prepare_metadata_for_build_wheel.out").write_text(dist_info, "utf-8")
    exit(0)

wheel_dir = work_dir / "build_wheel"
wheel_dir.mkdir()
wheel_basename = backend.build_wheel(
    str(wheel_dir),
    metadata_directory=str(metadata_dir) if metadata_dir.exists() else None,
)

(work_dir / "build_wheel.out").write_text(wheel_basename, "utf-8")
(work_dir / "build_wheel.binary_wheel_tag").write_text(binary_wheel_tag, "utf-8")
exit(0)