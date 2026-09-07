---
name = "Verifying a UI change on a branch"
brief = "Prove a UI change works before merging, by running your branch's own gateway on a spare port and driving headless Chrome over CDP."
when_to_use = "Use when a UI edit builds green but the running page does not show it, when curl on the live port 404s a file you just added to assets.rs, or when a change needs real browser evidence before it is merged to trunk. Also use when playwright MCP tools are unavailable and there is no node on the box. Not for reasoning about CSS or picking tokens; that is the parent skill."
tags = ["ui", "verify", "browser", "headless", "chrome", "cdp", "gateway", "branch", "404", "stale", "layout geometry", "console errors", "responsive behaviour", "gateways/gateway-web/src/ui", "assets.rs", "tool-group:shell", "tool-group:selfmod", "tool-group:browser"]
version = 6
---
