"""Configure a single Shipments backend through Home Assistant."""

from collections.abc import Mapping

import probatio as vol
from aiohttp import ClientError
from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.selector import (
    TextSelector,
    TextSelectorConfig,
    TextSelectorType,
)

from .client import InvalidBackendURL, ResponseTooLarge, ShipmentsClient
from .const import CONF_BACKEND_URL, CONF_TOKEN, DEFAULT_BACKEND_URL, DOMAIN


class ShipmentsConfigFlow(ConfigFlow, domain=DOMAIN):
    """Connect Home Assistant to an owner-authorized backend."""

    VERSION = 1

    async def async_step_user(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        """Check the supplied origin and credential before saving them."""
        if self._async_current_entries():
            return self.async_abort(reason="single_instance_allowed")
        return await self._async_configure(user_input)

    async def async_step_reauth(
        self, entry_data: Mapping[str, object]
    ) -> ConfigFlowResult:
        """Replace an expired backend credential on the existing entry."""
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        """Validate replacement credentials before updating the entry."""
        return await self._async_configure(user_input, reauth=True)

    async def _async_configure(
        self, user_input: dict[str, str] | None, *, reauth: bool = False
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                client = ShipmentsClient(
                    async_get_clientsession(self.hass),
                    user_input[CONF_BACKEND_URL],
                    user_input[CONF_TOKEN],
                )
                response = await client.request("GET", "/v1/system")
                if response.status in {401, 403}:
                    errors["base"] = "invalid_auth"
                elif response.status != 200:
                    errors["base"] = "cannot_connect"
                elif not isinstance(data := response.json(), dict):
                    errors["base"] = "cannot_connect"
                elif data.get("credential_role") != "owner":
                    errors["base"] = "owner_required"
                else:
                    values = {
                        CONF_BACKEND_URL: user_input[CONF_BACKEND_URL].rstrip("/"),
                        CONF_TOKEN: user_input[CONF_TOKEN],
                    }
                    if reauth:
                        return self.async_update_reload_and_abort(
                            self._get_reauth_entry(), data_updates=values
                        )
                    return self.async_create_entry(title="Shipments", data=values)
            except InvalidBackendURL:
                errors[CONF_BACKEND_URL] = "invalid_url"
            except ClientError, TimeoutError, ResponseTooLarge, ValueError:
                errors["base"] = "cannot_connect"

        default_url = (
            self._get_reauth_entry().data[CONF_BACKEND_URL]
            if reauth
            else DEFAULT_BACKEND_URL
        )
        return self.async_show_form(
            step_id="reauth_confirm" if reauth else "user",
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
