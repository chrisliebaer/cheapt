# CheaPT

Welcome to CheaPT, a Discord bot that interacts with the OpenAI API.
It's using generative models to generate text based on user input.
The bot is in a very early stage of development but already usable.

## Features

- Dynamic context management
- Flexible prompt customization using Tera templates
- Handling of Discord specific formatting

## Planned Features
- Per server prompt customization
- Message reporting
- Rate limiting
- Message caching
- Summarization for more context over multiple messages
- Tenor GIF support
- More permissions

## Environment Variables

The project requires the following environment variables:

- `OPENAI_TOKEN`: Your OpenAI API token.
- `DISCORD_TOKEN`: Your Discord bot token.
- `TEMPLATE_DIR`: The directory where your Tera templates are located. Defaults to `templates`.


## License

This project is licensed under the MIT license.
