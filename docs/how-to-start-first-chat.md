# How to Start Your First Chat

## Start a Session

When chatting with the bot 1:1 for the first time, type `/start`. This creates a temporary working directory where the bot can perform tasks.

When you send your first message after `/start`, an actual session is created on the coding agent, and each session is assigned a unique ID. You can check this ID with the `/session` command. You can also use this ID to resume the session directly from the coding agent's CLI.

## Check Available Models

Type `/model` to see the list of available models. The list reflects the agents actually installed on the system where cokacdir is running. Make sure the agent you want to use is installed beforehand.

## Set a Model

Type `/model [model name]` to set a model. Note that switching to a different model from the one currently in use will exit the current session.

## Check Working Directory

Type `/pwd` to see the current working directory path.

## Clear Conversation Context

Type `/clear` to discard the current session and start a new one. The previous session is not deleted — it is abandoned, and a fresh session begins.

## Optional Persistent Memory

Provider sessions normally carry the active conversation. If you want selective recall to survive `/clear`, session changes, provider changes, and working-directory changes, use the owner-only `/usememory` toggle for that chat. It is OFF by default and stores eligible completed User/final Assistant pairs as private plain-text records; it does not store tool execution details.

See [How to Use Persistent Conversation Memory](how-to-use-persistent-memory.md) before enabling it, especially the plain-text retention and privacy sections.
