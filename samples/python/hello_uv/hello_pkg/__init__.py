"""Python facade over the `hello` extension that Anubis builds.

An editable install leaves this package in the source tree, while the
native files land in site-packages/hello_pkg/_native/. extend_path adds
every `hello_pkg` directory on sys.path to this package, so `_native`
is found in either layout (and in a regular wheel install).
"""
from pkgutil import extend_path

__path__ = extend_path(__path__, __name__)

from hello_pkg._native.hello import add, greet  # noqa: E402

__all__ = ["add", "greet", "main"]


def main():
    import sys

    print(greet(sys.argv[1] if len(sys.argv) > 1 else "world"))
