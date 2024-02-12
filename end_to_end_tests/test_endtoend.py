import hashlib
import json
import os
import platform
from pathlib import Path
from subprocess import CalledProcessError, check_output
from typing import Any, Optional

import pytest
import glob
# import requests

from packse.scenario import load_scenarios, scenario_version

import pytest
from xprocess import ProcessStarter


class Packse:
    def __init__(self, path):
        self.path = path

    def path(self):
        return self.path

    def run_command(self, command):
        return check_output(["poetry", "run", "-C", str(self.path), *command]).decode(
            "utf-8"
        )

    def build_and_upload_scenario(self, name):
        self.run_command(
            ["packse", "build", f"{self.path}/scenarios/{name}.json", "--rm"]
        )

        for folder in glob.glob(f"./dist/*"):
            self.run_command(
                [
                    "packse",
                    "publish",
                    str(Path(folder).absolute()),
                    "--anonymous",
                    "--index-url",
                    "http://localhost:3141/packages/local",
                    "--skip-existing",
                ]
            )

        # # clear dist folder
        # for folder in glob.glob(f"./dist/*"):
        #     os.unlink(folder)

    def scenario(self, name):
        self.build_and_upload_scenario(name)
        path = self.path / "scenarios" / f"{name}.json"
        return load_scenarios(path)


@pytest.fixture
def packse():
    if os.environ.get("PACKSE_PATH"):
        return Packse(os.environ["PACKSE_PATH"])

    path = Path(__file__).parent.parent / "test-data/packse"
    return Packse(path)


@pytest.fixture
def packse_index(xprocess, packse: packse):
    class Starter(ProcessStarter):
        # startup pattern
        pattern = "Indexes available at http://localhost:3141/"

        # command to start process
        args = ["poetry", "run", "-C", str(packse.path), "packse", "index", "up"]

    # ensure process is running and return its logfile
    _logfile = xprocess.ensure("packse_index", Starter)

    yield "http://localhost:3141"

    # clean up whole process tree afterwards
    xprocess.getinfo("packse_index").terminate()


class Rip:
    def __init__(self, path):
        self.path = path

    def __call__(self, *args: Any, **kwds: Any) -> Any:
        try:
            return check_output([str(self.path), "resolve", *args], **kwds).decode("utf-8")
        except CalledProcessError as e:
            print(e.output)
            print(e.stderr)
            raise e

    def solve(self, args):
        output = self(*args, "--json")
        # find last "\n{" and remove everything before it
        lines = output.splitlines()
        last_index = -1
        for i, line in enumerate(lines):
            if line.startswith("{"):
                last_index = i
        j = "\n".join(lines[last_index:])
        return json.loads(j)


@pytest.fixture
def rip():
    if os.environ.get("RIP_PATH"):
        return Rip(os.environ["RIP_PATH"])
    else:
        base_path = Path(__file__).parent.parent
        executable_name = "rip"
        if os.name == "nt":
            executable_name += ".exe"

        cargo_build_target = os.environ.get("CARGO_BUILD_TARGET")
        if cargo_build_target:
            release_path = (
                base_path / f"target/{cargo_build_target}/release/{executable_name}"
            )
            debug_path = (
                base_path / f"target/{cargo_build_target}/debug/{executable_name}"
            )
        else:
            release_path = base_path / f"target/release/{executable_name}"
            debug_path = base_path / f"target/debug/{executable_name}"

        if release_path.exists():
            return Rip(release_path)
        elif debug_path.exists():
            return Rip(debug_path)

    raise FileNotFoundError("Could not find rip executable")


def test_functionality(rip: rip):
    text = rip("--help").splitlines()
    assert text[0] == "Resolve a set of requirements and output the resolved versions"


def test_solve(rip: rip):
    res = rip.solve(["numpy"])
    print(res)


def compare_packages(expected, s, hash):
    transformed_dict = dict()
    for k, v in expected.items():
        transformed_dict[f"{s.name}-{k}-{hash}"] = v
    return transformed_dict


def test_scenarios(packse: packse, rip: rip, packse_index: packse_index):
    scenario = packse.scenario("prereleases")
    errors = []
    success = []
    for s in scenario:
        h = scenario_version(s)

        enable_pre = s.environment.prereleases

        requested_packages = s.root.requires
        request = [str(p.with_unique_name(s, h, False)) for p in requested_packages]
        if enable_pre:
            request.append("--pre")
        request.append("--index-url")
        request.append(f"{packse_index}/packages/all/+simple")

        result = rip.solve(request)
        expected_packages = compare_packages(s.expected.packages, s, h)

        if result:
            if s.expected.satisfiable != result["resolved"]:
                error = {
                    "scenario": s.name,
                    "expected": s.expected.satisfiable,
                    "expected_packages": s.expected.packages,
                    "result": result,
                }
                errors.append(error)

            elif result["packages"] != expected_packages:
                error = {
                    "scenario": s.name,
                    "expected": s.expected.satisfiable,
                    "expected_packages": s.expected.packages,
                    "result": result,
                }
                errors.append(error)

            if (
                result["packages"] == expected_packages
                and s.expected.satisfiable == result["resolved"]
            ):
                success.append(s.name)

        else:
            errors.append(
                {
                    "scenario": s.name,
                    "expected": s.expected.satisfiable,
                    "expected_packages": s.expected.packages,
                    "result": "Could not run scenario!",
                }
            )

    print(f"Success: {len(success)}")
    print(f"Errors: {len(errors)}")

    if errors:
        print("\n\nErrors: ")
        print(errors)

    print("\n\nSucceeded: ")
    print(success)

    assert not errors
