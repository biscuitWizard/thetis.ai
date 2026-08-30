---
name = "Discord connector"
brief = "How the Discord bot connector works, and why its read-only guarantee rests on the session mode."
when_to_use = "Use when changing the Discord connector, adding another messaging surface (Slack, Telegram), or reasoning about what a chat surface is allowed to do. Also read it before exposing any new command over chat, because some commands would break the safety property."
tags = ["discord", "gateway", "security", "modes", "kernel", "tool-group:selfmod", "tool-group:config"]
children = "auto"
version = 4
---
