use anchor_lang::{system_program, InstructionData, ToAccountMetas};
use cryptohelpers::sha256;
use futures::future::join_all;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use shadow_drive_user_staking::accounts as shdw_drive_accounts;
use shadow_drive_user_staking::instruction as shdw_drive_instructions;
use solana_client::rpc_client::serialize_and_encode;
use solana_sdk::{
    instruction::Instruction, pubkey::Pubkey, signer::Signer, transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::collections::HashSet;
use std::fs::Metadata;
use std::io::SeekFrom;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncSeekExt;

use super::Client;
use crate::{
    constants::{PROGRAM_ADDRESS, SHDW_DRIVE_ENDPOINT, STORAGE_CONFIG_PDA, TOKEN_MINT, UPLOADER},
    derived_addresses,
    error::{Error, FileError},
    models::*,
};

/// UploadingData is a collection of info required for uploading a file
/// to Shadow Drive. Fields are generally derived from a given [`ShdwFile`] during the upload process.
#[derive(Debug)]
struct UploadingData {
    name: String,
    size: u64,
    sha256_hash: sha256::Sha256Hash,
    url: String,
    file: File,
}

impl<T> Client<T>
where
    T: Signer + Send + Sync,
{
    /// upload_multiple_files uploads a list of [`ShdwFile`]s to Shadow Drive.
    /// The multiple upload process is done in 4 steps:
    /// 1. Validate & prepare all files into [`UploadingData`]. If a file there are validation errors, the process is aborted.
    /// 2. Filter files that have the same name as a previously uploaded file. Uploads are not attempted for duplicates.
    /// 3. Divide files to be uploaded into batches of 5 or less to reduce calls but keep transaction size below the limit.
    /// 4. For each batch:
    ///   a. confirm file account seed
    ///   b. derive file account pubkey for each file
    ///   c. construct & partial sign transaction
    ///   d. submit transaction and files to Shadow Drive as multipart form data
    pub async fn upload_multiple_files(
        &self,
        storage_account_key: &Pubkey,
        data: Vec<ShdwFile>,
    ) -> ShadowDriveResult<Vec<ShadowBatchUploadResponse>> {
        let wallet_pubkey = self.wallet.pubkey();
        let (user_info, _) = derived_addresses::user_info(&wallet_pubkey);
        let selected_account = self.get_storage_account(storage_account_key).await?;

        //collect upload data for each file
        let upload_data_futures =
            data.into_iter()
                .map(|shdw_file| async move {
                    self.prepare_upload(shdw_file, storage_account_key).await
                })
                .collect::<Vec<_>>();

        let file_data = join_all(upload_data_futures).await;

        let (succeeded_files, errored_files): (Vec<_>, Vec<_>) =
            file_data.into_iter().partition(Result::is_ok);
        //it's safe to unwrap after the above partition
        let errored_files: Vec<Vec<FileError>> =
            errored_files.into_iter().map(Result::unwrap_err).collect();
        if errored_files.len() > 0 {
            return Err(Error::FileValidationError(
                errored_files.into_iter().flatten().collect(),
            ));
        }
        let succeeded_files = succeeded_files.into_iter().map(Result::unwrap);

        //filter out any existing files
        let all_objects: HashSet<String> = self
            .list_objects(&storage_account_key)
            .await?
            .into_iter()
            .collect();
        let (to_upload, existing_uploads): (Vec<_>, Vec<_>) = succeeded_files
            .into_iter()
            .partition(|file| !all_objects.contains(&file.name));

        //pre-fill results w/ existing files
        let mut upload_results = existing_uploads
            .into_iter()
            .map(|file| ShadowBatchUploadResponse {
                file_name: file.name,
                status: BatchUploadStatus::AlreadyExists,
                location: Some(file.url),
                transaction_signature: None,
            })
            .collect::<Vec<_>>();

        if upload_results.len() > 0 {
            tracing::debug!(existing_uploads = ?upload_results, "found existing files, will not attempt re-upload for existing files");
        }

        let mut batches = Vec::default();
        let mut current_batch: Vec<UploadingData> = Vec::default();
        let mut batch_total_name_length = 0;

        for file_data in to_upload {
            if current_batch.is_empty() {
                batch_total_name_length += file_data.name.as_bytes().len();
                current_batch.push(file_data);
                continue;
            }

            //if the current batch has 5 or less
            if current_batch.len() < 5 &&
            //our current name buffer is under the limit 
            batch_total_name_length < 154 &&
            //the name buffer will be under size with the new file
            batch_total_name_length + file_data.name.as_bytes().len() < 154
            {
                //add to current batch
                batch_total_name_length += file_data.name.as_bytes().len();
                current_batch.push(file_data);
            } else {
                //create new batch and clear name buffer
                batches.push(current_batch);
                current_batch = Vec::default();
                current_batch.push(file_data);
                batch_total_name_length = 0;
            }
        }
        //if the final batch has something, push it to batches
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        let mut new_file_seed = selected_account.init_counter;

        //send each batch to shdw drive
        for batch in batches {
            //confirm file seed before sending
            new_file_seed = self
                .confirm_storage_account_seed(new_file_seed, storage_account_key)
                .await?;

            let mut num_retries = 0;
            loop {
                match self
                    .send_batch(storage_account_key, user_info, &mut new_file_seed, &batch)
                    .await
                {
                    Ok(response) => {
                        upload_results.extend(response.into_iter());
                        //break loop to move to next batch
                        break;
                    }
                    Err(error) => {
                        tracing::error!(
                            retries = num_retries,
                            ?error,
                            "error uploading batch to shdw drive"
                        );
                        num_retries += 1;
                        //after 5 attempts bail on the batch
                        if num_retries == 5 {
                            //reset file seed
                            new_file_seed = self
                                .confirm_storage_account_seed(
                                    selected_account.init_counter,
                                    storage_account_key,
                                )
                                .await?;

                            //save failed entries
                            let failed = batch.into_iter().map(|file| ShadowBatchUploadResponse {
                                file_name: file.name,
                                status: BatchUploadStatus::Error(format!("{:?}", error)),
                                location: None,
                                transaction_signature: None,
                            });
                            upload_results.extend(failed);
                            //break batch retry loop to move to next
                            break;
                        }
                    }
                }
            }

            //wait 1/2 second before going to next batch
            //without this the latest blockhash doesn't align with the server's recent blockhash
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Ok(upload_results)
    }

    /// prepare_upload takes a [`ShdwFile`] and creates an [`UploadingData`] by:
    /// 1. retrieve file size & validate that it is below the 1GB limit
    /// 2. derive the expected upload URL from the `storage_account_key` and the file name,
    /// 3. calculate sha256 hash of the data in streaming fashion
    /// 4. validate that the file name is less than 32 bytes
    async fn prepare_upload(
        &self,
        mut shdw_file: ShdwFile,
        storage_account_key: &Pubkey,
    ) -> Result<UploadingData, Vec<FileError>> {
        let mut errors = Vec::new();
        let file_meta: Metadata;
        match shdw_file.file.metadata().await {
            Ok(meta) => file_meta = meta,
            Err(err) => {
                errors.push(FileError {
                    file: shdw_file.name.clone(),
                    error: format!("error opening file metadata: {:?}", err),
                });
                return Err(errors);
            }
        }
        let file_size = file_meta.len();
        if file_size > 1_073_741_824 {
            errors.push(FileError {
                file: shdw_file.name.clone(),
                error: String::from("Exceed the 1GB limit."),
            });
        }

        //this may need to be url encoded
        let url = format!(
            "https://shdw-drive.genesysgo.net/{}/{}",
            storage_account_key.to_string(),
            &shdw_file.name
        );

        //store any info about file bytes before moving into form
        let sha256_hash = match sha256::compute(&mut shdw_file.file).await {
            Ok(hash) => hash,
            Err(err) => {
                errors.push(FileError {
                    file: shdw_file.name.clone(),
                    error: format!("error hashing file: {:?}", err),
                });
                return Err(errors);
            }
        };

        if shdw_file.name.as_bytes().len() > 32 {
            errors.push(FileError {
                file: shdw_file.name.clone(),
                error: String::from("Exceed the 1GB limit."),
            });
        }

        if errors.len() > 0 {
            return Err(errors);
        }

        Ok(UploadingData {
            name: shdw_file.name,
            size: file_size,
            sha256_hash,
            url,
            file: shdw_file.file,
        })
    }

    /// confirm_storage_account_seed performs a retry loop to ensure that the file seed
    /// use to derive file account pubkeys is valid before creating a batch upload transaction.
    /// In the event that the chain has a more up to date file seed, the on-chain seed is used for the transaction.
    async fn confirm_storage_account_seed(
        &self,
        expected_seed: u32,
        storage_account_key: &Pubkey,
    ) -> ShadowDriveResult<u32> {
        let mut num_tries = 0;
        loop {
            let storage_account = self.get_storage_account(storage_account_key).await?;
            if expected_seed == storage_account.init_counter {
                tracing::debug!(
                    expected_seed,
                    actual_seed = storage_account.init_counter,
                    "Chain has up to date info. Moving onto the next batch."
                );
                return Ok(expected_seed);
            } else if expected_seed < storage_account.init_counter {
                tracing::debug!(
                    expected_seed,
                    actual_seed = storage_account.init_counter,
                    "Chain has higher seed. Fast forwarding to new start."
                );
                return Ok(storage_account.init_counter);
            } else {
                num_tries += 1;
                if num_tries == 300 {
                    // if we've tried for 5 minutes, give up
                    return Err(Error::InvalidStorage);
                }

                tracing::debug!(
                    expected_seed,
                    actual_seed = storage_account.init_counter,
                    "Chain does not have up to date info. Waiting 1s to check again."
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    /// send_batch constructs and partially signs a transaction for a given slice of [`UploadingData`].
    /// The transcation is then sent via HTTP POST to Shadow Drive servers as multipart form data alongside file contents.
    async fn send_batch(
        &self,
        storage_account_key: &Pubkey,
        user_info: Pubkey,
        new_file_seed: &mut u32,
        batch: &[UploadingData],
    ) -> ShadowDriveResult<Vec<ShadowBatchUploadResponse>> {
        //derive file account pubkeys using new_file_seed
        let mut files_with_pubkeys: Vec<(Pubkey, &UploadingData)> = Vec::with_capacity(batch.len());
        for file in batch {
            files_with_pubkeys.push((
                derived_addresses::file_account(&storage_account_key, *new_file_seed).0,
                file,
            ));
            *new_file_seed += 1;
        }

        //build txn
        let instructions = files_with_pubkeys
            .iter()
            .map(|(file_account, file)| {
                let accounts = shdw_drive_accounts::StoreFile {
                    storage_config: *STORAGE_CONFIG_PDA,
                    storage_account: *storage_account_key,
                    user_info,
                    owner: self.wallet.pubkey(),
                    uploader: UPLOADER,
                    token_mint: TOKEN_MINT,
                    system_program: system_program::ID,
                    file: *file_account,
                };
                let args = shdw_drive_instructions::StoreFile {
                    filename: file.name.clone(),
                    sha256_hash: hex::encode(file.sha256_hash.into_bytes()),
                    size: file.size,
                };
                Instruction {
                    program_id: PROGRAM_ADDRESS,
                    accounts: accounts.to_account_metas(None),
                    data: args.data(),
                }
            })
            .collect::<Vec<_>>();

        let mut txn =
            Transaction::new_with_payer(instructions.as_slice(), Some(&self.wallet.pubkey()));

        txn.try_partial_sign(&[&self.wallet], self.rpc_client.get_latest_blockhash()?)?;
        let txn_encoded = serialize_and_encode(&txn, UiTransactionEncoding::Base64)?;

        //construct HTTP form data
        let mut form = Form::new();
        for (_, file) in files_with_pubkeys {
            //because file is borrowed, we have to obtain a new instance.
            // `try_clone` uses the underlying file handle as the original
            let mut file_data = file
                .file
                .try_clone()
                .await
                .map_err(Error::FileSystemError)?;

            //seek to front of file
            file_data
                .seek(SeekFrom::Start(0))
                .await
                .map_err(Error::FileSystemError)?;

            form = form.part(
                "file",
                Part::stream_with_length(file_data, file.size).file_name(file.name.clone()),
            );
        }

        let form = form.part("transaction", Part::text(txn_encoded));

        //submit files to Shadow Drive
        let response = self
            .http_client
            .post(format!("{}/upload-batch", SHDW_DRIVE_ENDPOINT))
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Error::ShadowDriveServerError {
                status: response.status().as_u16(),
                message: response.json::<Value>().await?,
            });
        }

        //deserialize the response from ShadowDrive and return the upload details
        let response = response.json::<ShdwDriveBatchServerResponse>().await?;
        let response = batch
            .iter()
            .map(|file| ShadowBatchUploadResponse {
                file_name: file.name.clone(),
                status: BatchUploadStatus::Uploaded,
                location: Some(file.url.clone()),
                transaction_signature: Some(response.transaction_signature.clone()),
            })
            .collect();

        Ok(response)
    }
}