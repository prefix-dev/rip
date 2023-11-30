import sys
import json
import importlib
import compileall
from multiprocessing import Pool


def compile_one(path):
    success = compileall.compile_file(path, quiet=2, force=True)
    output_path = importlib.util.cache_from_source(path) if success else None
    return path, output_path


def compilation_finished(compilation_result):
    path, output_path = compilation_result
    print(json.dumps({"path": path, "output_path": output_path}))


if __name__ == "__main__":
    with sys.stdin:
        with Pool() as pool:
            while True:
                path = sys.stdin.readline().strip()
                if not path:
                    break
                pool.apply_async(compile_one, (path,), callback=compilation_finished)
