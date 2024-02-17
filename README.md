# CheaPT

Welcome to CheaPT, a Discord bot that interacts with the OpenAI API.
It's using generative models to generate text based on user input.
The bot is in a very early stage of development but already usable.

## Features

- Dynamic context management
- Flexible prompt customization using Tera templates
- Handling of Discord specific formatting
- Flexible rate limiting

## Planned Features
- Per server prompt customization
- Message reporting
- Message caching
- Summarization for more context over multiple messages
- Tenor GIF support

## Environment Variables

The project requires the following environment variables:

- `OPENAI_TOKEN`: Your OpenAI API token.
- `DISCORD_TOKEN`: Your Discord bot token.
- `TEMPLATE_DIR`: The directory where your Tera templates are located. Defaults to `templates`.
- `RATE_LIMIT_CONFIG`: The path to your rate limit configuration file. Defaults to `rate_limits.toml`.
- `DATABASE_URL`: The URL to your database. For example `mysql://user:password@localhost/database`.
- `WHITELIST_CHANNEL`: A comma separated list of channel IDs that the bot is allowed to respond in. If not set, the bot will respond in all channels.
- `OPT_OUT_LOCKOUT`: The time in seconds a user is locked out from the bot after opting out. Defaults to `30d`. Can use any time format supported by the `humantime` crate.


## License

This project is licensed under the MIT license.
