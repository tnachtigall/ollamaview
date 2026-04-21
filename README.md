# ollamaview
Custom Firefox / Librewolf AI Overview backend relying strictly on your local ollama models, written in Rust. <br>
Implemented right in your browser side panel, through the Firefox extension.

# dev setup:
Add 'com.(username).ollamaview.json' file with following content to project root, adding your name and path to cargo binary:
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