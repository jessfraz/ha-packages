"""Load the integration in standalone Core without Supervisor dependencies."""

import asyncio
import sys
from pathlib import Path


async def test_standalone_import_without_supervisor() -> None:
    """A clean interpreter must not need the optional hassio client package."""
    process = await asyncio.create_subprocess_exec(
        sys.executable,
        "-c",
        """
import importlib.abc
import sys

class MissingSupervisor(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path, target=None):
        if fullname == "aiohasupervisor" or fullname.startswith("aiohasupervisor."):
            raise ModuleNotFoundError(
                "Supervisor is not installed in standalone Core", name=fullname
            )

sys.meta_path.insert(0, MissingSupervisor())
import custom_components.shipments.config_flow
""",
        cwd=Path(__file__).parents[1],
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    try:
        async with asyncio.timeout(15):
            output, _ = await process.communicate()
        assert process.returncode == 0, output.decode()
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()
