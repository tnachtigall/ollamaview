# ollamaview
Custom Firefox / Librewolf AI Overview backend relying strictly on your local ollama models, written in Rust. Implemented right in your browser side panel, through the firefox extension.

# dev setup:
add 'com.<username>.ollamaview.json' file with following content:
```
{
    "name": "com.<username>.ollamaview",
    "description": "AI Overview for Firefox / Librewolf powered by local ollama instance",
    "path": "<absolute path to your cargo binary>",
    "type": "stdio",
    "allowed_extensions": [
        "{C76103A5-77D3-4F60-AB75-4B713E7FA15E}"
    ]
}
```