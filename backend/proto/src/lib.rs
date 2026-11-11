//! Generated protobuf bindings and restricted YAML conversion for Dotcore contracts.

#![expect(
    clippy::useless_borrows_in_formatting,
    reason = "prost and pbjson generated bindings included below emit formatting borrows"
)]

pub mod yaml;

/// Protobuf descriptor set generated from the contract source.
pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("dotcore-descriptor");

pub mod google {
    pub mod protobuf {
        tonic::include_proto!("google.protobuf");
        include!(concat!(env!("OUT_DIR"), "/google.protobuf.serde.rs"));
    }
}

pub mod dotcore {
    pub mod application {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.application.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.application.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.application.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.application.v1.serde.rs"));
        }
    }

    pub mod capacity {
        pub mod v1 {
            tonic::include_proto!("dotcore.capacity.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.capacity.v1.serde.rs"));
        }
    }

    pub mod catalog {
        pub mod v1 {
            tonic::include_proto!("dotcore.catalog.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.catalog.v1.serde.rs"));
        }
    }

    pub mod deployment {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.deployment.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.deployment.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.deployment.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.deployment.v1.serde.rs"));
        }
    }
    pub mod engine {
        pub mod llama_cpp {
            pub mod v1 {
                tonic::include_proto!("dotcore.engine.llama_cpp.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.engine.llama_cpp.v1.serde.rs"
                ));
            }
        }

        pub mod ninfer {
            pub mod v1 {
                tonic::include_proto!("dotcore.engine.ninfer.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.engine.ninfer.v1.serde.rs"
                ));
            }
        }

        pub mod strata {
            pub mod v1 {
                tonic::include_proto!("dotcore.engine.strata.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.engine.strata.v1.serde.rs"
                ));
            }
        }

        pub mod vllm {
            pub mod v1 {
                tonic::include_proto!("dotcore.engine.vllm.v1");
                include!(concat!(env!("OUT_DIR"), "/dotcore.engine.vllm.v1.serde.rs"));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.engine.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.engine.v1.serde.rs"));
        }
    }

    pub mod diagnostic {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.diagnostic.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.diagnostic.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.diagnostic.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.diagnostic.v1.serde.rs"));
        }
    }

    pub mod inference {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.inference.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.inference.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.inference.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.inference.v1.serde.rs"));
        }
    }

    pub mod launcher {
        pub mod store {
            pub mod v1 {
                tonic::include_proto!("dotcore.launcher.store.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.launcher.store.v1.serde.rs"
                ));
            }
        }
    }

    pub mod metrics {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.metrics.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.metrics.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.metrics.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.metrics.v1.serde.rs"));
        }
    }

    pub mod model {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.model.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.model.private.v1.serde.rs"
                ));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.model.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.model.v1.serde.rs"));
        }
    }

    pub mod node {
        pub mod v1 {
            tonic::include_proto!("dotcore.node.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.node.v1.serde.rs"));
        }
    }

    pub mod observation {
        pub mod v1 {
            tonic::include_proto!("dotcore.observation.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.observation.v1.serde.rs"));
        }
    }

    pub mod runtime {
        pub mod store {
            pub mod v1 {
                tonic::include_proto!("dotcore.runtime.store.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.runtime.store.v1.serde.rs"
                ));
            }
        }
    }

    pub mod site {
        pub mod private {
            pub mod v1 {
                tonic::include_proto!("dotcore.site.private.v1");
                include!(concat!(
                    env!("OUT_DIR"),
                    "/dotcore.site.private.v1.serde.rs"
                ));
            }
        }

        pub mod store {
            pub mod v1 {
                tonic::include_proto!("dotcore.site.store.v1");
                include!(concat!(env!("OUT_DIR"), "/dotcore.site.store.v1.serde.rs"));
            }
        }

        pub mod v1 {
            tonic::include_proto!("dotcore.site.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.site.v1.serde.rs"));
        }
    }

    pub mod telemetry {
        pub mod v1 {
            tonic::include_proto!("dotcore.telemetry.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.telemetry.v1.serde.rs"));
        }
    }

    pub mod watch {
        pub mod v1 {
            tonic::include_proto!("dotcore.watch.v1");
            include!(concat!(env!("OUT_DIR"), "/dotcore.watch.v1.serde.rs"));
        }
    }
}
