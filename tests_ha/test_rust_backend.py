"""Cross-language contract through real HA auth and the compiled Rust service."""

import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import timedelta
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientTimeout
from homeassistant.config_entries import SOURCE_HASSIO, SOURCE_USER, ConfigEntryState
from homeassistant.core import Context, HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.hassio import HassioServiceInfo
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    SUMMARY_KEYS,
)

OWNER_TOKEN = "test-owner-credential-at-least-32-characters"
PREFIX = "/api/shipments/v1"


@dataclass
class RustBackend:
    """A disposable real service with credentials excluded from test output."""

    origin: str
    owner_token: str = field(repr=False)
    integration_token: str | None = field(default=None, repr=False)


@pytest.fixture
async def rust_backend(
    hass: HomeAssistant,
    tmp_path: Path,
    unused_tcp_port: int,
    request: pytest.FixtureRequest,
    socket_enabled,
) -> AsyncIterator[RustBackend]:
    """Launch the actual binary with disposable SQLite and no provider access."""
    binary = Path(os.environ.get("SHIPMENTS_TEST_BINARY", "target/debug/shipments"))
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the backend first: cargo build --locked --bin shipments")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("SHIPMENTS_", "SUPERVISOR"))
        and key != "EASYPOST_API_KEY"
    }
    database = tmp_path / "shipments.sqlite3"
    arguments = [
        "--bind",
        f"127.0.0.1:{unused_tcp_port}",
        "--database",
        str(database),
    ]
    app_mode = getattr(request, "param", False)
    owner_token = None if app_mode else OWNER_TOKEN
    integration_token = None
    if app_mode:
        options = tmp_path / "options.json"
        await asyncio.to_thread(options.write_text, "{}")
        arguments.extend(("--options", str(options)))
    else:
        environment["SHIPMENTS_API_TOKEN"] = OWNER_TOKEN
    origin = f"http://127.0.0.1:{unused_tcp_port}"
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        *arguments,
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(5):
            while True:
                if process.returncode is not None:
                    pytest.fail("The Rust service exited during startup")
                if owner_token is None:
                    try:
                        credentials = json.loads(
                            await asyncio.to_thread(
                                (tmp_path / "credentials.json").read_text
                            )
                        )
                    except FileNotFoundError:
                        await asyncio.sleep(0.01)
                        continue
                    owner_token = credentials["owner"]
                    integration_token = credentials["integration"]
                try:
                    async with async_get_clientsession(hass).get(
                        f"{origin}/v1/system",
                        headers={"Authorization": f"Bearer {owner_token}"},
                        timeout=ClientTimeout(total=1),
                    ) as response:
                        if response.status == 200:
                            break
                except ClientError, TimeoutError:
                    pass
                await asyncio.sleep(0.01)
        yield RustBackend(origin, owner_token, integration_token)
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
    rust_backend: RustBackend,
) -> None:
    """Save pending parcels through HA, then edit and confirm without a provider."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {CONF_BACKEND_URL: rust_backend.origin, CONF_TOKEN: rust_backend.owner_token},
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    admin = await hass_client()
    user = await hass_client(hass_user_token)
    response = await user.post(
        f"{PREFIX}/shipments",
        json={
            "title": "Office supplies",
            "direction": "inbound",
            "source": {"namespace": "test", "key": "1"},
        },
        headers={"Idempotency-Key": "cross-language-create"},
    )
    assert response.status == 400
    response = await user.post(
        f"{PREFIX}/shipments",
        json={
            "title": "Office supplies",
            "direction": "inbound",
            "tracking": {"number": "TEST123", "carrier": "USPS"},
        },
        headers={"Idempotency-Key": "tracked-create-without-provider"},
    )
    assert response.status == 201
    shipment = await response.json()
    shipment_id = shipment["id"]
    assert shipment["status"] == "expected"
    assert shipment["sync"]["registration_state"] == "pending"
    assert shipment["sync"]["tracker_id"] is None
    added = await hass.services.async_call(
        DOMAIN,
        "add",
        {
            "title": "Second parcel",
            "direction": "outbound",
            "tracking_number": "TEST456",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    assert added["status"] == "expected"
    assert added["id"] != shipment_id
    response = await user.get(f"{PREFIX}/shipments/{added['id']}")
    assert (await response.json())["direction"] == "outbound"
    items = (await (await user.get(f"{PREFIX}/shipments")).json())["items"]
    assert {item["id"] for item in items} == {shipment_id, added["id"]}

    response = await user.get(f"{PREFIX}/shipments/{shipment_id}")
    assert response.status == 200
    shipment = await response.json()
    assert shipment["status"] == "expected"
    assert shipment["tracking"] == {"number": "TEST123", "carrier": "USPS"}
    assert shipment["direction"] == "inbound"
    response = await user.patch(
        f"{PREFIX}/shipments/{shipment_id}",
        json={"title": "Updated office supplies"},
        headers={"If-Match": response.headers["ETag"]},
    )
    assert response.status == 200
    shipment = await response.json()
    shipment_id = shipment["id"]
    etag = response.headers["ETag"]
    assert shipment["status"] == "expected"
    assert shipment["revision"] == 2
    # Burst writes share the integration's existing refresh cooldown.
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=11))
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_expected").state == "2"

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
    assert action_result == {"id": shipment_id, "revision": 3, "status": "delivered"}
    response = await admin.get(f"{PREFIX}/shipments/{shipment_id}")
    assert response.status == 200
    shipment = await response.json()
    assert shipment["status"] == "delivered"
    assert shipment["status_override"]["actor_id"] == f"ha:{hass_admin_user.id}"
    assert response.headers["ETag"] != etag
    # HA coalesces burst refreshes; advance its clock through the cooldown.
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=22))
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
        f"ha:{hass_user.id}",
        f"ha:{hass_admin_user.id}",
    ]


@pytest.mark.parametrize("rust_backend", [True], indirect=True, ids=["app"])
async def test_app_generated_pairing_and_loaded_check_in(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    """Pair an empty-options app without asking the user for any credential."""
    discovery_uuid = "019c6e27e55b73d187d84e01f1f75043"
    addon_slug = "testrepo_shipments"
    session = async_get_clientsession(hass)
    async with session.get(
        f"{rust_backend.origin}/v1/system",
        headers={"Authorization": f"Bearer {rust_backend.integration_token}"},
    ) as response:
        assert response.status == 200
        assert (await response.json())["credential_role"] == "integration"

    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_HASSIO},
        data=HassioServiceInfo(
            config={
                CONF_BACKEND_URL: rust_backend.origin,
                CONF_TOKEN: rust_backend.integration_token,
            },
            name="Shipments",
            slug=addon_slug,
            uuid=discovery_uuid,
        ),
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert not hass.config_entries.async_entries(DOMAIN)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    entry = result["result"]
    assert entry.state is ConfigEntryState.LOADED
    assert entry.unique_id == discovery_uuid
    assert entry.data[CONF_ADDON_SLUG] == addon_slug
    assert entry.data[CONF_BACKEND_URL] == rust_backend.origin
    for key in SUMMARY_KEYS:
        assert hass.states.get(f"sensor.shipments_{key}").state == "0"

    async with session.get(
        f"{rust_backend.origin}/v1/installation",
        headers={"Authorization": f"Bearer {rust_backend.owner_token}"},
    ) as response:
        assert response.status == 200
        installation = await response.json()
    assert installation["integration"]["status"] == "pass"
    assert installation["authentication"]["status"] == "pass"
    assert installation["backup"]["status"] == "unavailable"
    assert installation["last_backup"] is None
