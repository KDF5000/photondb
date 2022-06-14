# PhotonDB

This is an experimental project to explore how to build a high performance data store in Rust.

The first plan is to build an async runtime based on io_uring and a storage engine based on Bw-Tree. And then build a standalone server based on the runtime and storage engine.