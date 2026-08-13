import ese
import numpy as np
import torch
from sentence_transformers import SentenceTransformer


class OurSentenceTransformerWrapper:
    """sentence-transformers compatible wrapper around our encoder."""

    def __init__(self):
        self.max_seq_length = 512
        self._dimensions = ese.DIMENSIONS
        self.similarity_fn_name = "cosine"
        self.model_card_data = type(
            "CardData", (), {"set_evaluation_metrics": lambda *a, **k: None}
        )()

    def encode(
        self,
        sentences,
        batch_size=65536,
        show_progress_bar=False,
        normalize_embeddings=False,
        convert_to_numpy=True,
        **kwargs,
    ):
        return ese.encode(sentences)

    def get_sentence_embedding_dimension(self):
        return self._dimensions

    def similarity(self, a, b):
        import torch

        if isinstance(a, np.ndarray):
            a = torch.from_numpy(a)
        if isinstance(b, np.ndarray):
            b = torch.from_numpy(b)
        a = torch.nn.functional.normalize(a, p=2, dim=1)
        b = torch.nn.functional.normalize(b, p=2, dim=1)
        return a @ b.T

    def encode_query(self, query, **kwargs):
        return self.encode(query, **kwargs)

    def encode_document(self, document, **kwargs):
        return self.encode(document, **kwargs)


def load_model(model_name: str, require_cpu: bool = True):
    device = "mps" if (torch.backends.mps.is_available() and not require_cpu) else "cpu"
    if model_name == "ours":
        return OurSentenceTransformerWrapper()
    return SentenceTransformer(model_name, device=device, trust_remote_code=True)
