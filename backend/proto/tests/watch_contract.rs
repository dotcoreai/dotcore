//! Resource watch protobuf contract tests.
//!
//! These tests pin the watch package as an owner-neutral envelope: it declares only its stream
//! messages and carries each watched kind as its owner's own read. Hub behavior and transport are
//! covered by backend suites.

use dotcore_proto::FILE_DESCRIPTOR_SET;
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorProto, FileDescriptorSet};

#[test]
fn watch_package_declares_only_its_stream_envelope() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let watch = descriptor_file(&descriptor, "dotcore/watch/v1/service.proto");

    assert_eq!(
        watch
            .message_type
            .iter()
            .map(|message| message.name.as_deref().expect("message name"))
            .collect::<Vec<_>>(),
        [
            "WatchResourcesRequest",
            "WatchResourcesResponse",
            "ResourceSubscription",
            "KeyedResource",
        ]
    );
    assert!(watch.enum_type.is_empty());
}

#[test]
fn hardware_snapshot_kind_is_the_site_list_read() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let watch = descriptor_file(&descriptor, "dotcore/watch/v1/service.proto");

    assert_eq!(
        field_type(
            message(watch, "ResourceSubscription"),
            "hardware_snapshots",
            3
        ),
        ".dotcore.site.v1.ListHardwareSnapshotsRequest"
    );
    assert_eq!(
        field_type(message(watch, "KeyedResource"), "hardware_snapshots", 3),
        ".dotcore.site.v1.ListHardwareSnapshotsResponse"
    );
}

#[test]
fn node_memory_kind_is_the_capacity_list_read() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let watch = descriptor_file(&descriptor, "dotcore/watch/v1/service.proto");

    assert_eq!(
        field_type(message(watch, "ResourceSubscription"), "node_memory", 8),
        ".dotcore.capacity.v1.ListNodeMemoryRequest"
    );
    assert_eq!(
        field_type(message(watch, "KeyedResource"), "node_memory", 8),
        ".dotcore.capacity.v1.ListNodeMemoryResponse"
    );
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
}

fn message<'a>(file: &'a FileDescriptorProto, name: &str) -> &'a DescriptorProto {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{name} message must exist"))
}

/// The type of a message's field, checked to carry the expected wire number.
fn field_type<'a>(message: &'a DescriptorProto, name: &str, number: i32) -> &'a str {
    let field = message
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{name} field must exist"));
    assert_eq!(field.number, Some(number), "{name} field number");
    field.type_name.as_deref().expect("message field type name")
}
