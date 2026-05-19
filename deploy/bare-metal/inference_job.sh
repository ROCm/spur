#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Spur job wrapper: activate venv and run the TP inference test.
# Deployed to ~/spur/ on each cluster node.
source ~/spur/venv/bin/activate
exec python3 ~/spur/inference_test.py
