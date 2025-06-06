// Copyright (c) Microsoft Corporation. All rights reserved.
//
// Licensed under the MIT License. See License.txt in the project root for license information.
// Code generated by Microsoft (R) Rust Code Generator. DO NOT EDIT.

#![doc = include_str!("../README.md")]
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod clients;
mod generated;
mod pipeline;

pub use crate::clients::{BlobClient, BlobContainerClient, BlobServiceClient, BlockBlobClient};
pub use crate::generated::clients::{
    BlobClientOptions, BlobContainerClientOptions, BlobServiceClientOptions, BlockBlobClientOptions,
};
pub use crate::generated::models::{
    BlobClientDeleteOptions, BlobClientDownloadOptions, BlobClientGetPropertiesOptions,
    BlobClientSetMetadataOptions, BlobClientSetPropertiesOptions, BlobClientSetTierOptions,
    BlobContainerClientCreateOptions, BlobContainerClientDeleteOptions,
    BlobContainerClientGetPropertiesOptions, BlobContainerClientSetMetadataOptions,
    BlobServiceClientGetPropertiesOptions, BlockBlobClientCommitBlockListOptions,
    BlockBlobClientGetBlockListOptions, BlockBlobClientStageBlockOptions,
    BlockBlobClientUploadOptions,
};

pub mod models {
    pub use crate::generated::models::{
        AccessTier, ArchiveStatus, BlobClientDownloadResult, BlobClientDownloadResultHeaders,
        BlobClientGetPropertiesResult, BlobClientGetPropertiesResultHeaders,
        BlobContainerClientGetPropertiesResult, BlobContainerClientGetPropertiesResultHeaders,
        BlobImmutabilityPolicyMode, BlobType, BlockBlobClientCommitBlockListResult,
        BlockBlobClientStageBlockResult, BlockBlobClientUploadResult, BlockList, BlockListType,
        BlockLookupList, CopyStatus, LeaseState, LeaseStatus, PublicAccessType, RehydratePriority,
        StorageServiceProperties,
    };
}
