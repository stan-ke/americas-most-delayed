use crate::agency::AgencyConfig;

pub trait GtfsCatalogProvider {
    fn get_agencies(&self) -> Vec<AgencyConfig>;
}
