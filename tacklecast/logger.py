import logging
import os
import sys
from datetime import datetime
from logging.handlers import RotatingFileHandler


def _log_dir():
    """Return the log directory — _internal/logs for frozen builds, logs/ for dev."""
    if getattr(sys, 'frozen', False):
        base = os.path.join(os.path.dirname(sys.executable), "_internal", "logs")
    else:
        base = os.path.join(os.path.dirname(os.path.dirname(__file__)), "logs")
    os.makedirs(base, exist_ok=True)
    return base


def setup_logger():
    """Create and return the TackleCast logger with rotating file output."""
    logger = logging.getLogger("tacklecast")
    if logger.handlers:
        return logger

    logger.setLevel(logging.DEBUG)

    log_dir = _log_dir()
    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    log_file = os.path.join(log_dir, f"tacklecast_{timestamp}.log")

    handler = logging.FileHandler(log_file, encoding="utf-8")
    handler.setLevel(logging.DEBUG)
    formatter = logging.Formatter(
        "%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    handler.setFormatter(formatter)
    logger.addHandler(handler)

    # Prune old logs — keep only the 5 newest
    _prune_logs(log_dir, keep=5)

    return logger


def _prune_logs(log_dir, keep=5):
    """Delete oldest log files, keeping only the most recent `keep` files."""
    try:
        logs = sorted(
            [f for f in os.listdir(log_dir) if f.startswith("tacklecast_") and f.endswith(".log")],
            reverse=True,
        )
        for old in logs[keep:]:
            os.remove(os.path.join(log_dir, old))
    except Exception:
        pass


def get_logger():
    """Get the TackleCast logger (must call setup_logger first)."""
    return logging.getLogger("tacklecast")
