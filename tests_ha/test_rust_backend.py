"""Cross-language contract through real HA auth and the compiled Rust service."""

import asyncio
import os
from collections.abc import AsyncIterator
from datetime import timedelta
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientTimeout
from homeassistant.config_entries import SOURCE_USER
from homeassistant.core import Context, HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed

from custom_components.shipments.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN

OWNER_TOKEN = "test-owner-credential-at-least-32-characters"
AGENT_TOKEN = "test-agent-credential-at-least-32-characters"
PREFIX = "/api/shipments/v1"


@pytest.fixture
async def rust_backend(
    hass: HomeAssistant, tmp_path: Path, unused_tcp_port: int, socket_enabled
) -> AsyncIterator[str]:
    """Launch the actual binary with disposable SQLite and no provider access."""
    binary = Path(os.environ.get("SHIPMENTS_TEST_BINARY", "target/debug/shipments"))
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the backend first: cargo build --locked --bin shipments")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("SHIPMENTS_") and key != "EASYPOST_API_KEY"
    }
    environment["SHIPMENTS_API_TOKEN"] = OWNER_TOKEN
    environment["SHIPMENTS_AGENT_TOKEN"] = AGENT_TOKEN
    origin = f"http://127.0.0.1:{unused_tcp_port}"
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        "--bind",
        f"127.0.0.1:{unused_tcp_port}",
        "--database",
        str(tmp_path / "shipments.sqlite3"),
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(5):
            while True:
                if process.returncode is not None:
                    pytest.fail("The Rust service exited during startup")
                try:
                    async with async_get_clientsession(hass).get(
                        f"{origin}/v1/system",
                        headers={"Authorization": f"Bearer {OWNER_TOKEN}"},
                        timeout=ClientTimeout(total=1),
                    ) as response:
                        if response.status == 200:
                            break
                except ClientError, TimeoutError:
                    pass
                await asyncio.sleep(0.01)
        yield origin
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                async with asyncio.timeout(5):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()


async def test_ha_to_rust_contract(
    hass: HomeAssistant,
    hass_client,
    hass_admin_user,
    hass_user,
    hass_user_token: str,
    rust_backend: str,
) -> None:
    """Create, confirm, audit, and count a shipment across the actual boundary."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
        data={CONF_BACKEND_URL: rust_backend, CONF_TOKEN: OWNER_TOKEN},
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    admin = await hass_client()
    user = await hass_client(hass_user_token)
    response = await user.post(
        f"{PREFIX}/shipments",
        json={"title": "Office supplies", "source": {"namespace": "test", "key": "1"}},
        headers={"Idempotency-Key": "cross-language-create"},
    )
    assert response.status == 201
    shipment = await response.json()
    shipment_id = shipment["id"]
    etag = response.headers["ETag"]
    assert shipment["status"] == "expected"
    assert shipment["revision"] == 1
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_expected").state == "1"

    # Core authenticates the regular user; the bridge denies their confirmation.
    forbidden = await user.put(
        f"{PREFIX}/shipments/{shipment_id}/status-override",
        json={"status": "delivered", "reason": "Received in person"},
        headers={"If-Match": etag},
    )
    assert forbidden.status == 403

    # The native action uses the same Rust contract, ETag, and HA actor identity.
    action_result = await hass.services.async_call(
        DOMAIN,
        "set_override",
        {
            "shipment_id": shipment_id,
            "status": "delivered",
            "reason": "Received in person",
        },
        context=Context(user_id=hass_admin_user.id),
        blocking=True,
        return_response=True,
    )
    assert action_result == {"id": shipment_id, "revision": 2, "status": "delivered"}
    response = await admin.get(f"{PREFIX}/shipments/{shipment_id}")
    assert response.status == 200
    shipment = await response.json()
    assert shipment["status"] == "delivered"
    assert shipment["status_override"]["actor_id"] == f"ha:{hass_admin_user.id}"
    assert response.headers["ETag"] != etag
    # HA coalesces burst refreshes; advance its clock through the cooldown.
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=11))
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_delivered").state == "1"

    conflict = await admin.patch(
        f"{PREFIX}/shipments/{shipment_id}",
        json={"title": "Stale edit"},
        headers={"If-Match": etag},
    )
    assert conflict.status == 412
    events_response = await admin.get(f"{PREFIX}/shipments/{shipment_id}/events")
    assert events_response.status == 200
    events = (await events_response.json())["items"]
    assert [event["actor_id"] for event in events] == [
        f"ha:{hass_user.id}",
        f"ha:{hass_admin_user.id}",
    ]
    created = await hass.services.async_call(
        DOMAIN,
        "add",
        {"title": "Second parcel", "merchant": "Example shop"},
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    assert created["id"] != shipment_id
    assert created["revision"] == 1
    assert created["status"] == "expected"
    saved = await (await admin.get(f"{PREFIX}/shipments/{created['id']}")).json()
    assert saved["title"] == "Second parcel"
    assert saved["merchant"] == "Example shop"
