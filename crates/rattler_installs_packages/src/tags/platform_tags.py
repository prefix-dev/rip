import json
import sys

# The implementation has the packaging module vendored
from packaging.tags import sys_tags

json.dump([(tag.interpreter, tag.abi, tag.platform) for tag in sys_tags()], sys.stdout)
