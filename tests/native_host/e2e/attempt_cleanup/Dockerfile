# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

ARG BASE_IMAGE=ubuntu:22.04
FROM ${BASE_IMAGE}
USER 0
RUN apt-get update && apt-get install -y --no-install-recommends python3-pip util-linux procps \
    && python3 -m pip install --no-cache-dir grpcio==1.76.0 grpcio-tools==1.76.0 \
    && rm -rf /var/lib/apt/lists/*
COPY bin/ /opt/spur/
COPY slurm.proto probe.py /validation/
RUN python3 -m grpc_tools.protoc -I/validation --python_out=/validation \
    --grpc_python_out=/validation /validation/slurm.proto
ENV SPUR_STEPD_STATE_DIR=/state
USER 65534:65534
ENTRYPOINT ["python3", "/validation/probe.py"]
