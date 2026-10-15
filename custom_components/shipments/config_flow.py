"""Connect through authenticated app discovery or an explicit standalone origin."""

import asyncio
from collections.abc import Mapping
from uuid import UUID

import probatio as vol
from aiohasupervisor import SupervisorError
from aiohttp import ClientError
from homeassistant.components.hassio import get_supervisor_client
from homeassistant.config_entries import SOURCE_REAUTH, ConfigFlow, ConfigFlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.hassio import is_hassio
from homeassistant.helpers.selector import (
    TextSelector,
    TextSelectorConfig,
    TextSelectorType,
)
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from .client import (
    InvalidBackendURL,
    ResponseTooLarge,
    ShipmentsClient,
    normalize_backend_url,
)
from .const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DEFAULT_BACKEND_URL,
    DOMAIN,
    REQUEST_TIMEOUT_SECONDS,
)


class ShipmentsConfigFlow(ConfigFlow, domain=DOMAIN):
    """Keep app credentials on the authenticated Supervisor-to-Core path."""

    VERSION = 1

    def __init__(self) -> None:
        self._discovered: dict[str, str] | None = None

    async def async_step_user(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        """Offer app pairing without asking the user for a credential."""
        if self._async_current_entries():
            return self.async_abort(reason="single_instance_allowed")
        return self.async_show_menu(step_id="user", menu_options=["app", "standalone"])

    async def async_step_app(
        self, user_input: dict[str, object] | None = None
    ) -> ConfigFlowResult:
        """Find the running app through Home Assistant's Supervisor client."""
        error = "app_not_found"
        if not is_hassio(self.hass):
            error = "supervisor_required"
        else:
            try:
                async with asyncio.timeout(REQUEST_TIMEOUT_SECONDS):
                    discoveries = await get_supervisor_client(
                        self.hass
                    ).discovery.list()
                expected_slug = (
                    self._get_reauth_entry().data.get(CONF_ADDON_SLUG)
                    if self.source == SOURCE_REAUTH
                    else None
                )
                matches = [
                    item
                    for item in discoveries
                    if item.service == DOMAIN
                    and (expected_slug is None or item.addon == expected_slug)
                ]
                if len(matches) == 1:
                    discovered = matches[0]
                    return await self.async_step_hassio(
                        HassioServiceInfo(
                            config=discovered.config,
                            name="Shipments",
                            slug=discovered.addon,
                            uuid=discovered.uuid.hex,
                        )
                    )
                if len(matches) > 1:
                    error = "multiple_apps"
            except SupervisorError, ClientError, TimeoutError:
                error = "supervisor_unavailable"
        return self.async_show_form(
            step_id="app", data_schema=vol.Schema({}), errors={"base": error}
        )

    async def async_step_hassio(
        self, discovery_info: HassioServiceInfo
    ) -> ConfigFlowResult:
        """Stage trusted Supervisor discovery; confirmation precedes probing."""
        if not isinstance(discovery_info, HassioServiceInfo):
            return self.async_abort(reason="invalid_discovery")
        config = discovery_info.config
        if not isinstance(config, dict):
            return self.async_abort(reason="invalid_discovery")
        backend_url = config.get(CONF_BACKEND_URL)
        token = config.get(CONF_TOKEN)
        if (
            not isinstance(backend_url, str)
            or not isinstance(token, str)
            or not token
            or any(character.isspace() for character in token)
            or not isinstance(discovery_info.slug, str)
            or not discovery_info.slug
        ):
            return self.async_abort(reason="invalid_discovery")
        try:
            backend_url = normalize_backend_url(backend_url)
            unique_id = UUID(discovery_info.uuid).hex
        except ValueError, TypeError, AttributeError:
            return self.async_abort(reason="invalid_discovery")
        await self.async_set_unique_id(unique_id)
        if self.source == SOURCE_REAUTH:
            self._abort_if_unique_id_mismatch()
        else:
            self._abort_if_unique_id_configured()
            if self._async_current_entries():
                return self.async_abort(reason="single_instance_allowed")
        self._discovered = {
            CONF_BACKEND_URL: backend_url,
            CONF_TOKEN: token,
            CONF_ADDON_SLUG: discovery_info.slug,
        }
        self.context["title_placeholders"] = {"name": discovery_info.name}
        return await self.async_step_hassio_confirm()

    async def async_step_hassio_confirm(
        self, user_input: dict[str, object] | None = None
    ) -> ConfigFlowResult:
        """Confirm the app before sending its credential or storing it."""
        if self._discovered is None:
            return self.async_abort(reason="invalid_discovery")
        errors: dict[str, str] = {}
        if user_input is not None:
            if error := await self._async_validate(self._discovered, app=True):
                errors["base"] = error
            elif self.source == SOURCE_REAUTH:
                return self.async_update_reload_and_abort(
                    self._get_reauth_entry(), data_updates=self._discovered
                )
            else:
                return self.async_create_entry(title="Shipments", data=self._discovered)
        return self.async_show_form(
            step_id="hassio_confirm",
            data_schema=vol.Schema({}),
            errors=errors,
        )

    async def async_step_standalone(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        """Configure an independently hosted service explicitly."""
        return await self._async_configure(user_input)

    async def async_step_reauth(
        self, entry_data: Mapping[str, object]
    ) -> ConfigFlowResult:
        """Rediscover app credentials or reconnect an explicit standalone service."""
        if self._get_reauth_entry().data.get(CONF_ADDON_SLUG):
            return await self.async_step_app()
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        """Validate replacement standalone credentials before saving them."""
        return await self._async_configure(user_input, reauth=True)

    async def _async_validate(
        self, values: dict[str, str], *, app: bool = False
    ) -> str | None:
        try:
            client = ShipmentsClient(
                async_get_clientsession(self.hass),
                values[CONF_BACKEND_URL],
                values[CONF_TOKEN],
            )
            response = await client.request("GET", "/v1/system")
            if response.status in {401, 403}:
                return "invalid_auth"
            if response.status != 200 or not isinstance(data := response.json(), dict):
                return "cannot_connect"
            accepted_roles = {"integration"} if app else {"integration", "owner"}
            if data.get("credential_role") not in accepted_roles:
                return "integration_required"
        except InvalidBackendURL:
            return "invalid_url"
        except ClientError, TimeoutError, ResponseTooLarge, ValueError:
            return "cannot_connect"
        return None

    async def _async_configure(
        self, user_input: dict[str, str] | None, *, reauth: bool = False
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            if error := await self._async_validate(user_input):
                errors[CONF_BACKEND_URL if error == "invalid_url" else "base"] = error
            else:
                values = {
                    CONF_BACKEND_URL: normalize_backend_url(
                        user_input[CONF_BACKEND_URL]
                    ),
                    CONF_TOKEN: user_input[CONF_TOKEN],
                }
                if reauth:
                    return self.async_update_reload_and_abort(
                        self._get_reauth_entry(), data_updates=values
                    )
                return self.async_create_entry(title="Shipments", data=values)
        default_url = (
            self._get_reauth_entry().data[CONF_BACKEND_URL]
            if reauth
            else DEFAULT_BACKEND_URL
        )
        return self.async_show_form(
            step_id="reauth_confirm" if reauth else "standalone",
            data_schema=vol.Schema(
                {
                    vol.Required(CONF_BACKEND_URL, default=default_url): TextSelector(),
                    vol.Required(CONF_TOKEN): TextSelector(
                        TextSelectorConfig(type=TextSelectorType.PASSWORD)
                    ),
                }
            ),
            errors=errors,
        )
