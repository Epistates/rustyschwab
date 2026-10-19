"""The live smoke script's helpers, and its MCP client against the real
server in paper mode (no Schwab credentials, nothing leaves the machine)."""

import asyncio
import os
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from live_smoke import (
    ROOT,
    McpError,
    McpStdio,
    compare_tags,
    far_limit_price,
    held,
    open_buys,
    principal_policy,
    redact,
    risk_limits,
)


class HelperTests(unittest.TestCase):
    def test_far_limit_price_rounds_down_to_a_valid_tick(self):
        self.assertEqual(far_limit_price(Decimal("12.37")), Decimal("6.18"))
        self.assertEqual(far_limit_price(Decimal("1.50")), Decimal("0.7500"))
        self.assertEqual(far_limit_price(Decimal("0.01")), Decimal("0.01"))
        with self.assertRaises(ValueError):
            far_limit_price(Decimal(0))

    def test_redact_keeps_only_the_last_four(self):
        self.assertEqual(redact("acct ABCDEF123456 failed", ["ABCDEF123456"]), "acct …3456 failed")
        self.assertEqual(redact("nothing", ["", "zzzz"]), "nothing")

    def test_open_buys_include_children_and_skip_terminal_orders(self):
        buy = {"instruction": "BUY"}
        sell = {"instruction": "SELL"}
        orders = [
            {"status": "WORKING", "orderLegCollection": [buy]},
            {"status": "FILLED", "orderLegCollection": [buy]},
            {
                "status": "FILLED",
                "orderLegCollection": [sell],
                "childOrderStrategies": [{"status": "QUEUED", "orderLegCollection": [buy]}],
            },
            {"status": "WORKING", "orderLegCollection": [sell]},
        ]
        self.assertEqual(len(open_buys(orders)), 2)

    def test_held_sums_long_quantity_for_the_symbol(self):
        account = {
            "securitiesAccount": {
                "positions": [
                    {"instrument": {"symbol": "F"}, "longQuantity": 3},
                    {"instrument": {"symbol": "T"}, "longQuantity": 9},
                ]
            }
        }
        self.assertEqual(held(account, "F"), Decimal(3))
        self.assertEqual(held({}, "F"), Decimal(0))

    def test_compare_tags_reports_what_schwab_kept(self):
        self.assertEqual(compare_tags("rs-1", "rs-1"), "kept")
        with self.assertRaisesRegex(RuntimeError, "API_TRADER"):
            compare_tags("rs-1", "API_TRADER")
        with self.assertRaisesRegex(RuntimeError, "None"):
            compare_tags("rs-1", None)

    def test_limits_admit_one_share_and_no_more(self):
        policy = principal_policy("HASH", Decimal("6.18"))[0]
        self.assertEqual((policy["max_quantity"], policy["max_notional"]), ("1", "7.18"))
        self.assertNotIn("require_approval", policy)
        limits = risk_limits("HASH", Decimal("6.18"), Decimal(3))["HASH"]
        self.assertEqual(limits["max_open_quantity"], "1")
        self.assertEqual(limits["max_position_quantity"], "4")


BINARY = ROOT / "target" / "debug" / "rustyschwab-mcp"


@unittest.skipUnless(BINARY.exists(), "build it with cargo build -p rustyschwab-mcp")
class McpClientTests(unittest.TestCase):
    def test_handshake_calls_and_tool_errors_against_paper_mode(self):
        async def scenario(workdir: Path) -> None:
            env = {k: v for k, v in os.environ.items() if not k.startswith("SCHWAB_")}
            env.update(
                SCHWAB_MCP_PAPER_CONFIG=str(ROOT / "ops" / "paper" / "config.json"),
                SCHWAB_MCP_PAPER_STATE=str(workdir / "state" / "paper.sqlite3"),
            )
            # From the temp directory, so the server cannot find a
            # developer's .env and its live credentials.
            async with McpStdio.spawn(
                [str(BINARY)], env, workdir / "server.log", cwd=workdir
            ) as mcp:
                status = await mcp.call("get_server_status", {})
                self.assertEqual(status["mode"], "paper")
                order = await mcp.call(
                    "build_order",
                    {
                        "spec": {
                            "kind": "limit",
                            "price": "6.18",
                            "leg": {
                                "instruction": "BUY",
                                "quantity": "1",
                                "instrument": {"asset_type": "EQUITY", "symbol": "F"},
                            },
                        }
                    },
                )
                self.assertEqual((order["orderType"], order["price"]), ("LIMIT", "6.18"))
                with self.assertRaises(McpError) as refused:
                    await mcp.call(
                        "build_order",
                        {"spec": {"kind": "trigger", "entry": order_spec(), "children": []}},
                    )
                self.assertEqual(refused.exception.code, "invalid_arguments")

        with tempfile.TemporaryDirectory() as workdir:
            # Python falls back to the working directory when no temp
            # directory is usable, and the server reads .env from its
            # directory or any parent: never start it inside the repository.
            if Path(workdir).resolve().is_relative_to(ROOT):
                self.skipTest(f"temporary directory {workdir} is inside the repository")
            try:
                asyncio.run(scenario(Path(workdir)))
            except Exception:
                log = Path(workdir) / "server.log"
                print(log.read_text() if log.exists() else "no server log")
                raise


def order_spec() -> dict:
    return {
        "kind": "limit",
        "price": "6.18",
        "leg": {
            "instruction": "BUY",
            "quantity": "1",
            "instrument": {"asset_type": "EQUITY", "symbol": "F"},
        },
    }


if __name__ == "__main__":
    unittest.main()
