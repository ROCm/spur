#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Distributed PyTorch GPU test for Spur
# Submit: ~/spur/bin/sbatch -J dist-test -N 2 ~/spur/distributed_job.sh
source ~/spur/venv/bin/activate
exec python3 ~/spur/distributed_test.py
