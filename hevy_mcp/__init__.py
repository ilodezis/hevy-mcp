"""Hevy MCP — a standalone remote Model Context Protocol connector for Claude.ai.

Wraps the Hevy public REST API (https://api.hevyapp.com/docs/) as MCP tools so
Claude can read and *build* workouts and routines directly in a conversation.

Single user, no database — Hevy itself is the datastore. The OAuth 2.0 handshake
and Bearer-token MCP transport provide seamless integration.
"""

__version__ = "0.1.0"
