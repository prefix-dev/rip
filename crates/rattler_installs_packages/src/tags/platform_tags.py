import json
import sys
import platform

if sys.version_info < (3, 6):
    print(
        '"could not determine compatible interpreter tags, the python version is too old. '
        'Requires at least 3.6, but currently running %s"' % platform.python_version()
    )
    exit(0)

# The implementation has the packaging module vendored
from packaging.tags import sys_tags

json.dump([(tag.interpreter, tag.abi, tag.platform) for tag in sys_tags()], sys.stdout)
