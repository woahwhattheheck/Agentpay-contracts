/// Event topic constants.
///
/// NOTE: Events MUST use `symbol_short!` with a maximum of 9 characters.
/// Keep these consistent with existing indexer expectations.
pub const TOPIC_USAGE: Symbol = symbol_short!("usage");
pub const TOPIC_USAGE_HI: Symbol = symbol_short!("usage_hi");
pub const TOPIC_USAGE_DEC: Symbol = symbol_short!("usage_dec");
pub const TOPIC_SETTLED: Symbol = symbol_short!("settled");
pub const TOPIC_SETTL_ALL: Symbol = symbol_short!("settl_all");
pub const TOPIC_SETTLE_V: Symbol = symbol_short!("settle_v");
pub const TOPIC_PRICE_SET: Symbol = symbol_short!("price_set");
pub const TOPIC_PRICE_RMV: Symbol = symbol_short!("price_rmv");
pub const TOPIC_TIERS_SET: Symbol = symbol_short!("tiers_set");
pub const TOPIC_TIERS_RM: Symbol = symbol_short!("tiers_rm");
pub const TOPIC_PAUSED: Symbol = symbol_short!("paused");
pub const TOPIC_SVC_REG: Symbol = symbol_short!("svc_reg");
pub const TOPIC_SVC_ADD: Symbol = symbol_short!("svc_add");
pub const TOPIC_SVC_RM: Symbol = symbol_short!("svc_rm");
pub const TOPIC_SVC_DIS: Symbol = symbol_short!("svc_dis");
pub const TOPIC_CFG_SET: Symbol = symbol_short!("cfg_set");
pub const TOPIC_BND_SET: Symbol = symbol_short!("bnd_set");
pub const TOPIC_AGT_ALW: Symbol = symbol_short!("agt_alw");
pub const TOPIC_AGT_BLK: Symbol = symbol_short!("agt_blk");
pub const TOPIC_RATE_RST: Symbol = symbol_short!("rate_rst");
pub const TOPIC_ALERT_THR: Symbol = symbol_short!("alert_thr");
pub const TOPIC_REQ_REG: Symbol = symbol_short!("req_reg");
pub const TOPIC_ADMIN_CAN: Symbol = symbol_short!("admin_can");
pub const TOPIC_ADMIN_CHG: Symbol = symbol_short!("admin_chg");
pub const TOPIC_ADMIN_PRP: Symbol = symbol_short!("admin_prp");
pub const TOPIC_META_SET: Symbol = symbol_short!("meta_set");
pub const TOPIC_META_CLR: Symbol = symbol_short!("meta_clr");
pub const TOPIC_OWNER_CHG: Symbol = symbol_short!("owner_chg");
pub const TOPIC_CRED_DEB: Symbol = symbol_short!("cred_deb");
pub const TOPIC_DISPUTE: Symbol = symbol_short!("dispute");
pub const TOPIC_OPEN: Symbol = symbol_short!("open");
pub const TOPIC_RESOLVE: Symbol = symbol_short!("resolve");
