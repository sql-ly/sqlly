#!/usr/bin/env python3
"""MLX Sidecar: OpenAI-compatible HTTP server for local MLX inference."""

import argparse
import json
import os
import time
from typing import Any, Dict, List, Optional, Union

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, Field
from starlette.responses import JSONResponse

app = FastAPI(title="SQLLY MLX Sidecar", version="0.1.0")

# ---------------------------------------------------------------------------
# Model registry
# ---------------------------------------------------------------------------

class ModelSpec:
    def __init__(
        self,
        id: str,
        capabilities: List[str],
        context_window: Optional[int] = None,
        embedding_dimensions: Optional[int] = None,
        hf_repo: str = "",
    ):
        self.id = id
        self.capabilities = capabilities
        self.context_window = context_window
        self.embedding_dimensions = embedding_dimensions
        self.hf_repo = hf_repo

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "object": "model",
            "provider": "mlx",
            "capabilities": self.capabilities,
            "context_window": self.context_window,
            "embedding_dimensions": self.embedding_dimensions,
        }

# Known models with metadata. Chat models are loaded via mlx_lm; embedding
# models are loaded via sentence-transformers.
KNOWN_MODELS: Dict[str, ModelSpec] = {
    "qwen2.5-coder-7b-instruct-mlx": ModelSpec(
        id="qwen2.5-coder-7b-instruct-mlx",
        capabilities=["chat", "code", "sql"],
        context_window=32768,
        hf_repo="mlx-community/Qwen2.5-Coder-7B-Instruct-4bit",
    ),
    "bge-small-en-mlx": ModelSpec(
        id="bge-small-en-mlx",
        capabilities=["embedding"],
        embedding_dimensions=384,
        hf_repo="sentence-transformers/all-MiniLM-L6-v2",
    ),
    "qwen2.5-coder-3b-instruct-mlx": ModelSpec(
        id="qwen2.5-coder-3b-instruct-mlx",
        capabilities=["chat", "code", "sql"],
        context_window=32768,
        hf_repo="mlx-community/Qwen2.5-Coder-3B-Instruct-4bit",
    ),
    "nomic-embed-text-mlx": ModelSpec(
        id="nomic-embed-text-mlx",
        capabilities=["embedding"],
        embedding_dimensions=768,
        hf_repo="sentence-transformers/nomic-embed-text-v1",
    ),
}

# ---------------------------------------------------------------------------
# Lazy model loading
# ---------------------------------------------------------------------------

_loaded_models: Dict[str, Any] = {}


def _load_chat_model(model_id: str) -> Any:
    """Load a chat model via mlx_lm."""
    try:
        from mlx_lm import load, generate
    except ImportError:
        raise RuntimeError("mlx-lm is not installed. Install it with: pip install mlx-lm")

    # mlx_lm's sampling API moved temperature out of generate()/generate_step()
    # into an explicit sampler callable (make_sampler). Older versions accepted a
    # `temp=` kwarg directly; newer ones reject it. Probe for make_sampler so we
    # work across versions.
    try:
        from mlx_lm.sample_utils import make_sampler
    except ImportError:
        make_sampler = None

    spec = KNOWN_MODELS[model_id]
    model, tokenizer = load(spec.hf_repo)
    return {
        "model": model,
        "tokenizer": tokenizer,
        "generate": generate,
        "make_sampler": make_sampler,
    }


def _load_embedding_model(model_id: str) -> Any:
    """Load an embedding model via sentence-transformers."""
    try:
        from sentence_transformers import SentenceTransformer
    except ImportError:
        raise RuntimeError(
            "sentence-transformers is not installed. "
            "Install it with: pip install sentence-transformers"
        )

    spec = KNOWN_MODELS[model_id]
    model = SentenceTransformer(spec.hf_repo)
    return {"model": model}


def get_model(model_id: str) -> Any:
    """Lazy-load a model if not already loaded."""
    if model_id in _loaded_models:
        return _loaded_models[model_id]

    if model_id not in KNOWN_MODELS:
        raise HTTPException(
            status_code=404,
            detail=error_response(
                "model_not_found",
                f"Model '{model_id}' not found",
                {"model": model_id},
            ),
        )

    spec = KNOWN_MODELS[model_id]
    if "embedding" in spec.capabilities:
        loaded = _load_embedding_model(model_id)
    else:
        loaded = _load_chat_model(model_id)

    _loaded_models[model_id] = loaded
    return loaded


# ---------------------------------------------------------------------------
# Error helpers
# ---------------------------------------------------------------------------

def error_response(
    code: str, message: str, details: Optional[Dict[str, Any]] = None
) -> Dict[str, Any]:
    return {
        "error": {
            "type": code,
            "message": message,
            "code": code,
            "details": details or {},
        }
    }


class ErrorResponse(JSONResponse):
    def __init__(self, status_code: int, error: Dict[str, Any]):
        super().__init__(status_code=status_code, content=error)


# ---------------------------------------------------------------------------
# Request/response models
# ---------------------------------------------------------------------------

class ChatMessage(BaseModel):
    role: str
    content: str


class ChatCompletionRequest(BaseModel):
    model: str
    messages: List[ChatMessage]
    temperature: Optional[float] = 0.2
    max_tokens: Optional[int] = 1024


class EmbeddingRequest(BaseModel):
    model: str
    input: Union[str, List[str]]


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@app.get("/health")
async def health() -> Dict[str, Any]:
    return {
        "status": "ok",
        "provider": "mlx",
        "version": app.version,
        "loaded_models": list(_loaded_models.keys()),
        "errors": [],
    }


@app.get("/v1/models")
async def list_models() -> Dict[str, Any]:
    return {
        "object": "list",
        "data": [m.to_dict() for m in KNOWN_MODELS.values()],
    }


@app.post("/v1/chat/completions")
async def chat_completion(request: ChatCompletionRequest) -> Dict[str, Any]:
    spec = KNOWN_MODELS.get(request.model)
    if spec is None:
        raise HTTPException(
            status_code=404,
            detail=error_response(
                "model_not_found",
                f"Model '{request.model}' not found",
                {"model": request.model},
            ),
        )

    if "chat" not in spec.capabilities and "code" not in spec.capabilities:
        raise HTTPException(
            status_code=400,
            detail=error_response(
                "unsupported_model_capability",
                f"Model '{request.model}' does not support chat",
                {
                    "model": request.model,
                    "requested_capability": "chat",
                    "capabilities": spec.capabilities,
                },
            ),
        )

    try:
        loaded = get_model(request.model)
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(
            status_code=500,
            detail=error_response(
                "model_load_failure",
                f"Failed to load model '{request.model}': {str(e)}",
                {"model": request.model},
            ),
        )

    tokenizer = loaded["tokenizer"]
    generate = loaded["generate"]
    model = loaded["model"]
    make_sampler = loaded.get("make_sampler")

    # Build the prompt from messages
    messages = [
        {"role": m.role, "content": m.content} for m in request.messages
    ]
    try:
        prompt = tokenizer.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True
        )
    except Exception:
        # Fallback: simple concatenation
        prompt = "\n".join(
            f"{m.role}: {m.content}" for m in request.messages
        ) + "\nassistant:"

    try:
        temperature = request.temperature if request.temperature is not None else 0.2
        gen_kwargs = {
            "max_tokens": request.max_tokens or 1024,
        }
        if make_sampler is not None:
            # Current mlx_lm: temperature is supplied via a sampler callable.
            gen_kwargs["sampler"] = make_sampler(temp=temperature)
        else:
            # Legacy mlx_lm fallback: temperature passed directly.
            gen_kwargs["temp"] = temperature
        result = generate(
            model,
            tokenizer,
            prompt=prompt,
            **gen_kwargs,
        )
    except Exception as e:
        raise HTTPException(
            status_code=500,
            detail=error_response(
                "inference_failure",
                f"Inference failed: {str(e)}",
                {"model": request.model},
            ),
        )

    prompt_tokens = len(tokenizer.encode(prompt))
    completion_tokens = len(tokenizer.encode(result))

    return {
        "id": f"chatcmpl-local-{int(time.time() * 1000)}",
        "object": "chat.completion",
        "model": request.model,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": result},
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    }


@app.post("/v1/embeddings")
async def embeddings(request: EmbeddingRequest) -> Dict[str, Any]:
    spec = KNOWN_MODELS.get(request.model)
    if spec is None:
        raise HTTPException(
            status_code=404,
            detail=error_response(
                "model_not_found",
                f"Model '{request.model}' not found",
                {"model": request.model},
            ),
        )

    if "embedding" not in spec.capabilities:
        raise HTTPException(
            status_code=400,
            detail=error_response(
                "unsupported_model_capability",
                f"Model '{request.model}' does not support embeddings",
                {
                    "model": request.model,
                    "requested_capability": "embedding",
                    "capabilities": spec.capabilities,
                },
            ),
        )

    try:
        loaded = get_model(request.model)
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(
            status_code=500,
            detail=error_response(
                "model_load_failure",
                f"Failed to load model '{request.model}': {str(e)}",
                {"model": request.model},
            ),
        )

    model = loaded["model"]
    texts = request.input if isinstance(request.input, list) else [request.input]

    try:
        embeddings = model.encode(texts)
    except Exception as e:
        raise HTTPException(
            status_code=500,
            detail=error_response(
                "embedding_failure",
                f"Embedding failed: {str(e)}",
                {"model": request.model},
            ),
        )

    data = [
        {
            "object": "embedding",
            "index": i,
            "embedding": emb.tolist(),
        }
        for i, emb in enumerate(embeddings)
    ]

    return {
        "object": "list",
        "model": request.model,
        "data": data,
        "usage": {
            "prompt_tokens": sum(len(t.split()) for t in texts),
            "total_tokens": sum(len(t.split()) for t in texts),
        },
    }


# ---------------------------------------------------------------------------
# Exception handler
# ---------------------------------------------------------------------------

@app.exception_handler(HTTPException)
async def http_exception_handler(_, exc: HTTPException):
    if isinstance(exc.detail, dict) and "error" in exc.detail:
        return JSONResponse(status_code=exc.status_code, content=exc.detail)
    return JSONResponse(
        status_code=exc.status_code,
        content=error_response("http_error", str(exc.detail)),
    )


# ---------------------------------------------------------------------------
# Entrypoint
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="SQLLY MLX Sidecar")
    parser.add_argument("--host", default=os.environ.get("MLX_SIDECAR_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("MLX_SIDECAR_PORT", "18123")))
    args = parser.parse_args()

    import uvicorn
    uvicorn.run(app, host=args.host, port=args.port)


if __name__ == "__main__":
    main()
