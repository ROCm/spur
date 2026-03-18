#!/bin/bash
# Distributed PyTorch GPU test for Spur
# Submit: ~/spur/bin/sbatch -J dist-test -N 2 ~/spur/distributed_job.sh
source ~/spur/venv/bin/activate
exec python3 ~/spur/distributed_test.py
