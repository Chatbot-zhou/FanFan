from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
from pathlib import Path


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def open_read_only_database(database: Path) -> sqlite3.Connection:
    uri = f"file:{database.as_posix()}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=0.2)
    try:
        connection.execute("SELECT COUNT(*) FROM sqlite_master").fetchone()
        return connection
    except sqlite3.OperationalError:
        connection.close()
        if database.with_name(f"{database.name}-wal").exists():
            raise
        return sqlite3.connect(
            f"file:{database.as_posix()}?mode=ro&immutable=1",
            uri=True,
            timeout=0.2,
        )


def inspect_database(database: Path, sample_size: int) -> dict[str, object]:
    resolved = database.resolve(strict=True)
    connection = open_read_only_database(resolved)
    try:
        counts = {
            table: connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
            for table in (
                "roots",
                "files",
                "file_revisions",
                "chunks",
                "chunk_embeddings",
                "jobs",
            )
        }
        counts["pending_files"] = connection.execute(
            "SELECT COUNT(*) FROM files WHERE parse_status IN "
            "('pending','parsing','ocr_pending') AND availability = 'present'"
        ).fetchone()[0]
        counts["present_files"] = connection.execute(
            "SELECT COUNT(*) FROM files WHERE availability = 'present'"
        ).fetchone()[0]
        rows = connection.execute(
            "SELECT f.file_id, f.canonical_path FROM files f "
            "WHERE f.availability = 'present' AND EXISTS ("
            "SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id "
            "WHERE m.file_id = f.file_id AND r.enabled = 1) "
            "ORDER BY f.file_id LIMIT ?",
            (sample_size,),
        ).fetchall()
    finally:
        connection.close()

    source_hashes = []
    for file_id, source_path in rows:
        source = Path(source_path)
        if source.is_file():
            source_hashes.append((file_id, hash_file(source)))
    aggregate = hashlib.sha256(
        "\n".join(f"{file_id}:{digest}" for file_id, digest in source_hashes).encode()
    ).hexdigest()
    return {
        "database_size_bytes": os.path.getsize(resolved),
        "counts": counts,
        "sample_hash_count": len(source_hashes),
        "sample_hash_aggregate": aggregate,
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Read-only Remin database inventory and privacy-safe source hash sample."
    )
    parser.add_argument("database", type=Path)
    parser.add_argument("--sample-size", type=int, default=32)
    args = parser.parse_args()
    if not 1 <= args.sample_size <= 256:
        raise SystemExit("sample size must be between 1 and 256")
    print(json.dumps(inspect_database(args.database, args.sample_size), ensure_ascii=False))


if __name__ == "__main__":
    main()
