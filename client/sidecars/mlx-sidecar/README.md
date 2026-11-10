# MLX Sidecar API

A lightweight HTTP sidecar that exposes MLX models via an OpenAI-compatible REST API.

## Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Health check with model list |
| GET | `/v1/models` | List available models with capabilities |
| POST | `/v1/chat/completions` | Chat completions |
| POST | `/v1/embeddings` | Text embeddings |

## Running

```bash
# Install dependencies
pip install -r requirements.txt

# Start the server
python main.py
```

The server binds to `127.0.0.1:18123` by default. Override with `--port` or `--host`.

## Model Registry

Models are loaded lazily on first request. The first request to a model may be slow while weights are loaded into memory.

## mlx-lm sampling API

Chat completions call `mlx_lm.generate(...)`. Newer `mlx-lm` releases removed the
`temp=` keyword from `generate()`/`generate_step()` and require temperature to be
supplied via a sampler callable (`mlx_lm.sample_utils.make_sampler(temp=...)`).
The sidecar probes for `make_sampler` at load time and uses it when present,
falling back to the legacy `temp=` kwarg only on older versions. If you upgrade
`mlx-lm` and chat returns an HTTP 500 mentioning an unexpected `temp` argument,
that means an older sidecar process is still running — restart it.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MLX_SIDECAR_PORT` | `18123` | HTTP port |
| `MLX_SIDECAR_HOST` | `127.0.0.1` | HTTP host |
| `MLX_MODEL_DIR` | `~/.mlx_models` | Model cache directory |

## Error Responses

All errors follow the OpenAI-style format:

```json
{
  "error": {
    "type": "unsupported_model_capability",
    "message": "Model bge-small-en-mlx does not support chat",
    "code": "unsupported_model_capability",
    "details": {
      "model": "bge-small-en-mlx",
      "requested_capability": "chat",
      "capabilities": ["embedding"]
    }
  }
}
```