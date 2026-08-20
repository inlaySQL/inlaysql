# The driver image: Python, DuckDB, a PostgreSQL client and a MySQL client.
#
# Versions are pinned so a benchmark rerun a year from now measures the same
# thing. Bumping one is a deliberate act that shows up in the diff next to the
# numbers it changed.
FROM python:3.12-slim

RUN pip install --no-cache-dir \
      duckdb==1.1.3 \
      "psycopg[binary]==3.2.3" \
      mysql-connector-python==9.1.0

WORKDIR /drivers
COPY common.py duckdb_driver.py pgvector_driver.py postgres_oltp_driver.py mysql_driver.py \
     server_driver.py report.py ./

# The corpus is mounted, never baked in: the image is the environment and the
# corpus is the experiment.
VOLUME /corpus
ENV CORPUS=/corpus
