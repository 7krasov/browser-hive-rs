pub mod coordinator {
    tonic::include_proto!("scraper.coordinator");
}

pub mod worker {
    tonic::include_proto!("scraper.worker");
}
