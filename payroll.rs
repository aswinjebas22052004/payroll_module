use axum::{extract::{Extension, Path, State}, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use crate::{
    errors::{AppError, AppResult},
    middleware::{auth::AuthUser, tenant::TenantContext},
    services::payroll_calculator::{
        self, AdjustmentInputItem, AttendanceSummary, SalarySnapshot,
    },
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct CreatePayrollMonthRequest {
    pub year: i32,
    pub month: i32,
    pub working_days: i32,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddAdjustmentRequest {
    pub name: String,
    pub r#type: String,
    pub amount_type: String,
    pub amount: f64,
    pub applicable_to: String,
    pub role_ids: Option<Vec<Uuid>>,
    pub member_ids: Option<Vec<Uuid>>,
    pub note: Option<String>,
}

// ── POST /payroll/months ──────────────────────────────────────────────────────

pub async fn create_month(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
    Json(req): Json<CreatePayrollMonthRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = Uuid::new_v4();
    sqlx::query!(
        r#"INSERT INTO payroll_months (id, tenant_id, year, month, working_days, notes)
           VALUES ($1,$2,$3,$4,$5,$6)
           ON CONFLICT (tenant_id, year, month) DO UPDATE
             SET working_days=$5, notes=$6, updated_at=now()"#,
        id, tenant.tenant_id, req.year, req.month, req.working_days, req.notes
    )
    .execute(&state.db).await?;

    Ok((StatusCode::CREATED, Json(json!({
        "id": id, "year": req.year, "month": req.month,
        "working_days": req.working_days, "status": "draft",
        "message": format!("Payroll month {}/{} created", req.month, req.year),
    }))))
}

// ── GET /payroll/months ───────────────────────────────────────────────────────

pub async fn list_months(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
) -> AppResult<Json<Vec<Value>>> {
    let rows = sqlx::query!(
        "SELECT id, year, month, status, working_days, processed_at, paid_at, created_at
         FROM payroll_months WHERE tenant_id=$1 ORDER BY year DESC, month DESC",
        tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    Ok(Json(rows.into_iter().map(|r| json!({
        "id": r.id, "year": r.year, "month": r.month,
        "status": r.status, "working_days": r.working_days,
        "processed_at": r.processed_at, "paid_at": r.paid_at,
        "created_at": r.created_at,
    })).collect()))
}

// ── POST /payroll/months/:id/adjustments ─────────────────────────────────────

pub async fn add_adjustment(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
    Json(req): Json<AddAdjustmentRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    sqlx::query_scalar!(
        "SELECT id FROM payroll_months WHERE id=$1 AND tenant_id=$2",
        month_id, tenant.tenant_id
    )
    .fetch_optional(&state.db).await?
    .ok_or_else(|| AppError::NotFound("Payroll month not found".into()))?;

    let id         = Uuid::new_v4();
    let role_ids   = req.role_ids.unwrap_or_default();
    let member_ids = req.member_ids.unwrap_or_default();

    // Cast $6 from FLOAT8 → NUMERIC in SQL so no bigdecimal needed in Rust
    sqlx::query!(
        r#"INSERT INTO payroll_adjustments
               (id, payroll_month_id, name, type, amount_type, amount,
                applicable_to, role_ids, member_ids, note)
           VALUES ($1,$2,$3,$4,$5, ($6::TEXT)::NUMERIC ,$7,$8,$9,$10)"#,
        id, month_id, req.name, req.r#type, req.amount_type,
        req.amount.to_string(),          // bind as TEXT, cast to NUMERIC in SQL
        req.applicable_to, &role_ids, &member_ids, req.note
    )
    .execute(&state.db).await?;

    Ok((StatusCode::CREATED, Json(json!({
        "id": id, "name": req.name, "type": req.r#type,
        "amount": req.amount, "applicable_to": req.applicable_to,
    }))))
}

// ── GET /payroll/months/:id/adjustments ──────────────────────────────────────

pub async fn list_adjustments(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(_tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
) -> AppResult<Json<Vec<Value>>> {
    let rows = sqlx::query!(
        r#"SELECT id, name, type, amount_type,
                  amount::FLOAT8 AS "amount!: f64",
                  applicable_to, role_ids, member_ids, note
           FROM payroll_adjustments WHERE payroll_month_id=$1"#,
        month_id
    )
    .fetch_all(&state.db).await?;

    Ok(Json(rows.into_iter().map(|r| json!({
        "id": r.id, "name": r.name, "type": r.r#type,
        "amount_type": r.amount_type, "amount": r.amount,
        "applicable_to": r.applicable_to,
        "role_ids": r.role_ids, "member_ids": r.member_ids,
        "note": r.note,
    })).collect()))
}

// ── DELETE /payroll/months/:id/adjustments/:adj_id ────────────────────────────

pub async fn delete_adjustment(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(_tenant): Extension<TenantContext>,
    Path((_, adj_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    sqlx::query!("DELETE FROM payroll_adjustments WHERE id=$1", adj_id)
        .execute(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── POST /payroll/months/:id/generate ────────────────────────────────────────

pub async fn generate_payslips(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let pm = sqlx::query!(
        "SELECT year, month, working_days FROM payroll_months WHERE id=$1 AND tenant_id=$2",
        month_id, tenant.tenant_id
    )
    .fetch_optional(&state.db).await?
    .ok_or_else(|| AppError::NotFound("Payroll month not found".into()))?;

    let from_date = chrono::NaiveDate::from_ymd_opt(pm.year, pm.month as u32, 1).unwrap();
    let to_date   = chrono::NaiveDate::from_ymd_opt(pm.year, (pm.month + 1) as u32, 1)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(pm.year + 1, 1, 1).unwrap())
        - chrono::Duration::days(1);

    let members = sqlx::query!(
        r#"SELECT tm.user_id, r.id AS role_id
           FROM tenant_memberships tm
           JOIN roles r ON r.id = tm.role_id
           WHERE tm.tenant_id=$1"#,
        tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    let adjustments = sqlx::query!(
        r#"SELECT id, name, type, amount_type,
                  amount::FLOAT8 AS "amount!: f64",
                  applicable_to, role_ids, member_ids, note
           FROM payroll_adjustments WHERE payroll_month_id=$1"#,
        month_id
    )
    .fetch_all(&state.db).await?;

    // Recurring adjustments (tenant-level, auto-apply each month)
    let recurring_adjs = sqlx::query!(
        r#"SELECT name, type, amount_type, amount::FLOAT8 AS "amount!: f64",
                  applicable_to, role_ids, member_ids, note
           FROM recurring_adjustments WHERE tenant_id=$1 AND is_active=true"#,
        tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    // Late-fine rule (0 = disabled)
    let late_fine_per_occurrence = sqlx::query_scalar!(
        r#"SELECT late_fine_per_occurrence::FLOAT8 AS "f!: f64"
           FROM payroll_settings WHERE tenant_id=$1"#,
        tenant.tenant_id
    )
    .fetch_optional(&state.db).await?
    .unwrap_or(0.0);

    let mut generated = 0;
    let mut errors    = vec![];

    for member in &members {
        // Salary — cast NUMERIC → FLOAT8 in SQL, type-override to f64
        let salary = sqlx::query!(
            r#"SELECT pay_type,
                      ctc::FLOAT8           AS "ctc: f64",
                      basic::FLOAT8         AS "basic: f64",
                      da::FLOAT8            AS "da: f64",
                      hra::FLOAT8           AS "hra: f64",
                      special_allowance::FLOAT8 AS "special_allowance: f64",
                      daily_rate::FLOAT8    AS "daily_rate: f64",
                      hourly_rate::FLOAT8   AS "hourly_rate: f64",
                      is_pf_applicable,
                      pf_rate::FLOAT8       AS "pf_rate!: f64",
                      pf_on_basic_plus_da, pf_apply_ceiling,
                      is_esi_applicable, state_code, city_type
               FROM salary_structures
               WHERE member_id=$1 AND tenant_id=$2 AND effective_from <= $3
               ORDER BY effective_from DESC LIMIT 1"#,
            member.user_id, tenant.tenant_id, from_date
        )
        .fetch_optional(&state.db).await?;

        let Some(sal) = salary else {
            errors.push(json!({ "user_id": member.user_id, "error": "No salary structure found" }));
            continue;
        };

        let salary_snapshot = SalarySnapshot {
            pay_type:          sal.pay_type.clone(),
            ctc:               sal.ctc,
            basic:             sal.basic,
            da:                sal.da,
            hra:               sal.hra,
            special_allowance: sal.special_allowance,
            daily_rate:        sal.daily_rate,
            hourly_rate:       sal.hourly_rate,
            is_pf_applicable:  sal.is_pf_applicable,
            pf_rate:           sal.pf_rate,
            pf_on_basic_plus_da: sal.pf_on_basic_plus_da,
            pf_apply_ceiling:  sal.pf_apply_ceiling,
            is_esi_applicable: sal.is_esi_applicable,
            state_code:        sal.state_code.clone(),
            city_type:         sal.city_type.clone(),
        };

        let att = sqlx::query!(
            r#"SELECT
                 COUNT(DISTINCT date) FILTER (WHERE status='present')      AS days_present,
                 COUNT(DISTINCT date) FILTER (WHERE status='absent')       AS days_absent,
                 COUNT(DISTINCT date) FILTER (WHERE status='half_day')     AS days_half_day,
                 COUNT(DISTINCT date) FILTER (WHERE status='paid_leave')   AS days_paid_leave,
                 COUNT(DISTINCT date) FILTER (WHERE status='unpaid_leave') AS days_unpaid_leave,
                 COUNT(DISTINCT date) FILTER (WHERE status='holiday')      AS days_holiday,
                 COUNT(DISTINCT date) FILTER (WHERE status='week_off')     AS days_week_off,
                 COALESCE(SUM(working_minutes),0)              AS total_working_minutes,
                 COALESCE(SUM(total_break_minutes),0)          AS total_break_minutes,
                 COALESCE(SUM(overtime_minutes),0)             AS overtime_minutes
               FROM attendance_records
               WHERE member_id=$1 AND tenant_id=$2 AND date BETWEEN $3 AND $4"#,
            member.user_id, tenant.tenant_id, from_date, to_date
        )
        .fetch_one(&state.db).await?;

        let attendance = AttendanceSummary {
            working_days_in_month: pm.working_days,
            days_present:       att.days_present.unwrap_or(0)      as i32,
            days_absent:        att.days_absent.unwrap_or(0)       as i32,
            days_half_day:      att.days_half_day.unwrap_or(0)     as i32,
            days_paid_leave:    att.days_paid_leave.unwrap_or(0)   as i32,
            days_unpaid_leave:  att.days_unpaid_leave.unwrap_or(0) as i32,
            days_holiday:       att.days_holiday.unwrap_or(0)      as i32,
            days_week_off:      att.days_week_off.unwrap_or(0)     as i32,
            total_working_minutes: att.total_working_minutes.unwrap_or(0) as i32,
            total_break_minutes:   att.total_break_minutes.unwrap_or(0)   as i32,
            overtime_minutes:      att.overtime_minutes.unwrap_or(0)      as i32,
        };

        let mut applicable_adjs: Vec<AdjustmentInputItem> = adjustments.iter()
            .filter(|adj| match adj.applicable_to.as_str() {
                "all"    => true,
                "member" => adj.member_ids.as_ref()
                    .map(|ids| ids.contains(&member.user_id)).unwrap_or(false),
                "role"   => adj.role_ids.as_ref()
                    .map(|ids| ids.contains(&member.role_id)).unwrap_or(false),
                _ => false,
            })
            .map(|adj| AdjustmentInputItem {
                name:        adj.name.clone(),
                adj_type:    adj.r#type.clone(),
                amount_type: adj.amount_type.clone(),
                amount:      adj.amount,
                note:        adj.note.clone(),
            })
            .collect();

        // Recurring adjustments — auto-apply every month, same targeting rules
        for adj in &recurring_adjs {
            let applies = match adj.applicable_to.as_str() {
                "all"    => true,
                "member" => adj.member_ids.as_ref()
                    .map(|ids| ids.contains(&member.user_id)).unwrap_or(false),
                "role"   => adj.role_ids.as_ref()
                    .map(|ids| ids.contains(&member.role_id)).unwrap_or(false),
                _ => false,
            };
            if applies {
                applicable_adjs.push(AdjustmentInputItem {
                    name:        adj.name.clone(),
                    adj_type:    adj.r#type.clone(),
                    amount_type: adj.amount_type.clone(),
                    amount:      adj.amount,
                    note:        adj.note.clone().or_else(|| Some("Recurring".into())),
                });
            }
        }

        // Late-arrival fine — per occurrence × number of late days this month
        if late_fine_per_occurrence > 0.0 {
            let late_count = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "c!" FROM attendance_records
                   WHERE member_id=$1 AND tenant_id=$2 AND date BETWEEN $3 AND $4
                     AND is_late = true"#,
                member.user_id, tenant.tenant_id, from_date, to_date)
                .fetch_one(&state.db).await?;
            if late_count > 0 {
                applicable_adjs.push(AdjustmentInputItem {
                    name:        "Late arrival fine".into(),
                    adj_type:    "deduction".into(),
                    amount_type: "fixed".into(),
                    amount:      late_fine_per_occurrence * late_count as f64,
                    note:        Some(format!("{} late arrival(s)", late_count)),
                });
            }
        }

        // Advance recovery — sum of recoveries recorded against this payroll month
        let adv_recovery = sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(ar.amount),0)::FLOAT8 AS "total!: f64"
               FROM advance_recoveries ar
               JOIN advances a ON a.id = ar.advance_id
               WHERE a.member_id=$1 AND a.tenant_id=$2 AND ar.payroll_month_id=$3"#,
            member.user_id, tenant.tenant_id, month_id)
            .fetch_one(&state.db).await?;
        if adv_recovery > 0.0 {
            applicable_adjs.push(AdjustmentInputItem {
                name:        "Advance recovery".into(),
                adj_type:    "deduction".into(),
                amount_type: "fixed".into(),
                amount:      adv_recovery,
                note:        Some("Recovered from outstanding advance".into()),
            });
        }

        let calc = payroll_calculator::calculate_payslip(
            &salary_snapshot, &attendance, &applicable_adjs);

        let salary_snapshot_json = serde_json::to_value(&salary_snapshot).unwrap();

        // Bind all f64 money values as TEXT and cast to NUMERIC in SQL —
        // avoids bigdecimal feature while keeping NUMERIC column type
        let ps_id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO payslips (
                id, member_id, payroll_month_id, tenant_id,
                days_present, days_absent, days_half_day,
                days_paid_leave, days_unpaid_leave, days_holiday, days_week_off,
                total_working_minutes, total_break_minutes, overtime_minutes,
                basic_earned, hra_earned, special_allowance_earned, overtime_amount,
                adjustments_additions, total_additions, gross_earnings,
                employee_pf, employee_esi, professional_tax,
                adjustments_deductions, total_deductions, net_pay,
                employer_pf, employer_esi, salary_snapshot,
                status, generated_at
            ) VALUES (
                $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,
                ($15::TEXT)::NUMERIC,($16::TEXT)::NUMERIC,
                ($17::TEXT)::NUMERIC,($18::TEXT)::NUMERIC,
                $19,
                ($20::TEXT)::NUMERIC,($21::TEXT)::NUMERIC,
                ($22::TEXT)::NUMERIC,($23::TEXT)::NUMERIC,($24::TEXT)::NUMERIC,
                $25,
                ($26::TEXT)::NUMERIC,($27::TEXT)::NUMERIC,
                ($28::TEXT)::NUMERIC,($29::TEXT)::NUMERIC,
                $30,'generated', now()
            )
            ON CONFLICT (member_id, payroll_month_id) DO UPDATE SET
                days_present=$5,  days_absent=$6,    days_half_day=$7,
                days_paid_leave=$8, days_unpaid_leave=$9,
                days_holiday=$10, days_week_off=$11,
                total_working_minutes=$12, total_break_minutes=$13, overtime_minutes=$14,
                basic_earned=($15::TEXT)::NUMERIC, hra_earned=($16::TEXT)::NUMERIC,
                special_allowance_earned=($17::TEXT)::NUMERIC,
                overtime_amount=($18::TEXT)::NUMERIC,
                adjustments_additions=$19,
                total_additions=($20::TEXT)::NUMERIC,
                gross_earnings=($21::TEXT)::NUMERIC,
                employee_pf=($22::TEXT)::NUMERIC, employee_esi=($23::TEXT)::NUMERIC,
                professional_tax=($24::TEXT)::NUMERIC,
                adjustments_deductions=$25,
                total_deductions=($26::TEXT)::NUMERIC,
                net_pay=($27::TEXT)::NUMERIC,
                employer_pf=($28::TEXT)::NUMERIC, employer_esi=($29::TEXT)::NUMERIC,
                salary_snapshot=$30, status='generated', generated_at=now(),
                updated_at=now()"#,
            ps_id, member.user_id, month_id, tenant.tenant_id,
            calc.days_present,  calc.days_absent,    calc.days_half_day,
            calc.days_paid_leave, calc.days_unpaid_leave,
            calc.days_holiday,  calc.days_week_off,
            calc.total_working_minutes, calc.total_break_minutes, calc.overtime_minutes,
            calc.basic_earned.to_string(),             calc.hra_earned.to_string(),
            calc.special_allowance_earned.to_string(), calc.overtime_amount.to_string(),
            serde_json::to_value(&calc.adjustments_additions).unwrap(),
            calc.total_additions.to_string(),    calc.gross_earnings.to_string(),
            calc.employee_pf.to_string(),        calc.employee_esi.to_string(),
            calc.professional_tax.to_string(),
            serde_json::to_value(&calc.adjustments_deductions).unwrap(),
            calc.total_deductions.to_string(),   calc.net_pay.to_string(),
            calc.employer_pf.to_string(),        calc.employer_esi.to_string(),
            salary_snapshot_json,
        )
        .execute(&state.db).await?;

        generated += 1;
    }

    sqlx::query!(
        "UPDATE payroll_months SET status='processing', processed_at=now() WHERE id=$1",
        month_id
    )
    .execute(&state.db).await?;

    Ok(Json(json!({
        "generated": generated,
        "errors":    errors,
        "message":   format!("{} payslips generated for {}/{}", generated, pm.month, pm.year),
    })))
}

// ── GET /payroll/months/:id/payslips ─────────────────────────────────────────

pub async fn list_payslips(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
) -> AppResult<Json<Vec<Value>>> {
    let rows = sqlx::query!(
        r#"SELECT ps.id, ps.member_id, ps.status,
                  ps.days_present, ps.days_absent, ps.days_half_day,
                  ps.days_paid_leave, ps.days_unpaid_leave,
                  ps.total_working_minutes, ps.total_break_minutes, ps.overtime_minutes,
                  ps.basic_earned::FLOAT8             AS "basic_earned!: f64",
                  ps.hra_earned::FLOAT8               AS "hra_earned!: f64",
                  ps.special_allowance_earned::FLOAT8 AS "special_allowance_earned!: f64",
                  ps.overtime_amount::FLOAT8          AS "overtime_amount!: f64",
                  ps.adjustments_additions,
                  ps.total_additions::FLOAT8          AS "total_additions!: f64",
                  ps.gross_earnings::FLOAT8           AS "gross_earnings!: f64",
                  ps.employee_pf::FLOAT8              AS "employee_pf!: f64",
                  ps.employee_esi::FLOAT8             AS "employee_esi!: f64",
                  ps.professional_tax::FLOAT8         AS "professional_tax!: f64",
                  ps.adjustments_deductions,
                  ps.total_deductions::FLOAT8         AS "total_deductions!: f64",
                  ps.net_pay::FLOAT8                  AS "net_pay!: f64",
                  ps.employer_pf::FLOAT8              AS "employer_pf!: f64",
                  ps.employer_esi::FLOAT8             AS "employer_esi!: f64",
                  ps.generated_at, u.email, u.display_name, r.name AS role_name
           FROM payslips ps
           JOIN users u ON u.id = ps.member_id
           JOIN tenant_memberships tm
             ON tm.user_id = ps.member_id AND tm.tenant_id = ps.tenant_id
           JOIN roles r ON r.id = tm.role_id
           WHERE ps.payroll_month_id=$1 AND ps.tenant_id=$2
           ORDER BY r.rank ASC, u.display_name ASC"#,
        month_id, tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    Ok(Json(rows.into_iter().map(|r| json!({
        "id": r.id, "member_id": r.member_id, "status": r.status,
        "email": r.email, "display_name": r.display_name, "role_name": r.role_name,
        "attendance": {
            "days_present": r.days_present, "days_absent": r.days_absent,
            "days_half_day": r.days_half_day, "days_paid_leave": r.days_paid_leave,
            "days_unpaid_leave": r.days_unpaid_leave,
            "total_working_minutes": r.total_working_minutes,
            "total_break_minutes": r.total_break_minutes,
            "overtime_minutes": r.overtime_minutes,
        },
        "earnings": {
            "basic": r.basic_earned, "hra": r.hra_earned,
            "special_allowance": r.special_allowance_earned,
            "overtime": r.overtime_amount,
            "adjustments": r.adjustments_additions,
            "total_additions": r.total_additions, "gross": r.gross_earnings,
        },
        "deductions": {
            "employee_pf": r.employee_pf, "employee_esi": r.employee_esi,
            "professional_tax": r.professional_tax,
            "adjustments": r.adjustments_deductions, "total": r.total_deductions,
        },
        "net_pay": r.net_pay,
        "employer": { "pf": r.employer_pf, "esi": r.employer_esi },
        "generated_at": r.generated_at,
    })).collect()))
}

// ── GET /payroll/my-payslips ──────────────────────────────────────────────────

pub async fn my_payslips(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
) -> AppResult<Json<Vec<Value>>> {
    let rows = sqlx::query!(
        r#"SELECT ps.id, pm.year, pm.month, ps.status,
                  ps.days_present, ps.days_absent, ps.days_paid_leave,
                  ps.total_working_minutes, ps.total_break_minutes, ps.overtime_minutes,
                  ps.basic_earned::FLOAT8             AS "basic_earned!: f64",
                  ps.hra_earned::FLOAT8               AS "hra_earned!: f64",
                  ps.special_allowance_earned::FLOAT8 AS "special_allowance_earned!: f64",
                  ps.overtime_amount::FLOAT8          AS "overtime_amount!: f64",
                  ps.adjustments_additions,
                  ps.total_additions::FLOAT8          AS "total_additions!: f64",
                  ps.gross_earnings::FLOAT8           AS "gross_earnings!: f64",
                  ps.employee_pf::FLOAT8              AS "employee_pf!: f64",
                  ps.employee_esi::FLOAT8             AS "employee_esi!: f64",
                  ps.professional_tax::FLOAT8         AS "professional_tax!: f64",
                  ps.adjustments_deductions,
                  ps.total_deductions::FLOAT8         AS "total_deductions!: f64",
                  ps.net_pay::FLOAT8                  AS "net_pay!: f64",
                  ps.salary_snapshot, ps.generated_at
           FROM payslips ps
           JOIN payroll_months pm ON pm.id = ps.payroll_month_id
           WHERE ps.member_id=$1 AND ps.tenant_id=$2
           ORDER BY pm.year DESC, pm.month DESC"#,
        auth.user_id, tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    for row in &rows {
        if row.status == "generated" {
            let _ = sqlx::query!(
                "UPDATE payslips SET status='viewed', updated_at=now() WHERE id=$1", row.id
            ).execute(&state.db).await;
        }
    }

    Ok(Json(rows.into_iter().map(|r| json!({
        "id": r.id, "year": r.year, "month": r.month, "status": r.status,
        "attendance": {
            "days_present": r.days_present, "days_absent": r.days_absent,
            "days_paid_leave": r.days_paid_leave,
            "total_working_minutes": r.total_working_minutes,
            "total_break_minutes": r.total_break_minutes,
            "overtime_minutes": r.overtime_minutes,
        },
        "earnings": {
            "basic": r.basic_earned, "hra": r.hra_earned,
            "special_allowance": r.special_allowance_earned,
            "overtime": r.overtime_amount,
            "adjustments": r.adjustments_additions,
            "total_additions": r.total_additions, "gross": r.gross_earnings,
        },
        "deductions": {
            "employee_pf": r.employee_pf, "employee_esi": r.employee_esi,
            "professional_tax": r.professional_tax,
            "adjustments": r.adjustments_deductions, "total": r.total_deductions,
        },
        "net_pay": r.net_pay,
        "salary_snapshot": r.salary_snapshot, "generated_at": r.generated_at,
    })).collect()))
}

// ── POST /payroll/months/:id/approve ─────────────────────────────────────────

pub async fn approve_payroll(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(_tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    sqlx::query!(
        "UPDATE payroll_months SET status='approved', approved_at=now() WHERE id=$1", month_id
    ).execute(&state.db).await?;
    Ok(Json(json!({ "message": "Payroll approved" })))
}

// ── POST /payroll/months/:id/mark-paid ────────────────────────────────────────

pub async fn mark_paid(
    State(state): State<AppState>,
    Extension(_auth): Extension<AuthUser>,
    Extension(_tenant): Extension<TenantContext>,
    Path(month_id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    sqlx::query!(
        "UPDATE payroll_months SET status='paid', paid_at=now() WHERE id=$1", month_id
    ).execute(&state.db).await?;
    sqlx::query!(
        "UPDATE payslips SET status='acknowledged', updated_at=now() WHERE payroll_month_id=$1",
        month_id
    ).execute(&state.db).await?;
    Ok(Json(json!({ "message": "Payroll marked as paid" })))
}

// ── GET /dashboard/summary — role-aware analytics ────────────────────────────
// Returns admin aggregates if the caller has payroll:admin/attendance:admin,
// otherwise personal stats. One call powers the whole dashboard.
pub async fn dashboard_summary(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthUser>,
    Extension(tenant): Extension<TenantContext>,
) -> AppResult<Json<Value>> {
    let now   = chrono::Local::now();
    let year  = now.format("%Y").to_string().parse::<i32>().unwrap();
    let month = now.format("%m").to_string().parse::<u32>().unwrap();
    let from  = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let to    = chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap())
        - chrono::Duration::days(1);

    // Does this user have admin permissions?
    let is_admin = sqlx::query_scalar!(
        r#"SELECT EXISTS(
             SELECT 1 FROM tenant_memberships tm
             JOIN role_permissions rp ON rp.role_id = tm.role_id
             JOIN permissions p ON p.id = rp.permission_id
             WHERE tm.user_id=$1 AND tm.tenant_id=$2
               AND p.key IN ('payroll:admin','attendance:admin')
           ) AS "exists!: bool""#,
        auth.user_id, tenant.tenant_id
    )
    .fetch_one(&state.db).await?;

    // ── Personal attendance this month (everyone gets this) ───────────────────
    let my_att = sqlx::query!(
        r#"SELECT
             COUNT(DISTINCT date) FILTER (WHERE status='present')    AS present,
             COUNT(DISTINCT date) FILTER (WHERE status='absent')     AS absent,
             COUNT(DISTINCT date) FILTER (WHERE status='paid_leave') AS paid_leave,
             COALESCE(SUM(working_minutes),0)            AS working_minutes,
             COALESCE(SUM(overtime_minutes),0)           AS overtime_minutes
           FROM attendance_records
           WHERE member_id=$1 AND tenant_id=$2 AND date BETWEEN $3 AND $4"#,
        auth.user_id, tenant.tenant_id, from, to
    )
    .fetch_one(&state.db).await?;

    // ── My recent payslips (last 6 months) for the earnings trend ─────────────
    let my_slips = sqlx::query!(
        r#"SELECT pm.year, pm.month, ps.net_pay::FLOAT8 AS "net_pay!: f64"
           FROM payslips ps
           JOIN payroll_months pm ON pm.id = ps.payroll_month_id
           WHERE ps.member_id=$1 AND ps.tenant_id=$2
           ORDER BY pm.year DESC, pm.month DESC LIMIT 6"#,
        auth.user_id, tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    let personal = json!({
        "attendance": {
            "present":    my_att.present.unwrap_or(0),
            "absent":     my_att.absent.unwrap_or(0),
            "paid_leave": my_att.paid_leave.unwrap_or(0),
            "working_minutes":  my_att.working_minutes.unwrap_or(0),
            "overtime_minutes": my_att.overtime_minutes.unwrap_or(0),
        },
        "payslip_trend": my_slips.iter().rev().map(|s| json!({
            "year": s.year, "month": s.month, "net_pay": s.net_pay,
        })).collect::<Vec<_>>(),
        "latest_payslip": my_slips.first().map(|s| json!({
            "year": s.year, "month": s.month, "net_pay": s.net_pay,
        })),
    });

    if !is_admin {
        return Ok(Json(json!({ "role": "member", "personal": personal })));
    }

    // ── Admin aggregates ──────────────────────────────────────────────────────
    let today = now.date_naive();
    let team = sqlx::query!(
        r#"SELECT
             COUNT(DISTINCT tm.user_id)                                   AS "total!: i64",
             COUNT(DISTINCT ar.member_id) FILTER (WHERE ar.clock_in_at IS NOT NULL) AS "present!: i64",
             COUNT(DISTINCT ar.member_id) FILTER (WHERE ar.is_late)       AS "late!: i64"
           FROM tenant_memberships tm
           LEFT JOIN attendance_records ar
             ON ar.member_id = tm.user_id AND ar.tenant_id = tm.tenant_id AND ar.date = $2
             WHERE tm.tenant_id = $1
               AND tm.deactivated_at IS NULL"#,
        tenant.tenant_id, today
    )
    .fetch_one(&state.db).await?;

    // Members by role (composition donut)
    let by_role = sqlx::query!(
        r#"SELECT r.name AS role_name, COUNT(*) AS "count!: i64"
           FROM tenant_memberships tm
           JOIN roles r ON r.id = tm.role_id
           WHERE tm.tenant_id = $1
             AND tm.deactivated_at IS NULL
           GROUP BY r.name ORDER BY COUNT(*) DESC"#,
        tenant.tenant_id
    )
    .fetch_all(&state.db).await?;

    // This month's payroll totals (if generated)
    let payroll = sqlx::query!(
        r#"SELECT
             COALESCE(SUM(ps.net_pay),0)::FLOAT8        AS "net!: f64",
             COALESCE(SUM(ps.gross_earnings),0)::FLOAT8 AS "gross!: f64",
             COALESCE(SUM(ps.employer_pf + ps.employer_esi),0)::FLOAT8 AS "employer!: f64",
             COUNT(*)                                   AS "count!: i64"
           FROM payslips ps
           JOIN payroll_months pm ON pm.id = ps.payroll_month_id
           WHERE ps.tenant_id=$1 AND pm.year=$2 AND pm.month=$3"#,
        tenant.tenant_id, year, month as i32
    )
    .fetch_one(&state.db).await?;

    let admin = json!({
        "today": {
            "total":   team.total,
            "present": team.present,
            "absent":  team.total - team.present,
            "late":    team.late,
        },
        "by_role": by_role.iter().map(|r| json!({
            "role": r.role_name, "count": r.count,
        })).collect::<Vec<_>>(),
        "payroll_this_month": {
            "net":      payroll.net,
            "gross":    payroll.gross,
            "employer": payroll.employer,
            "cost_to_company": payroll.gross + payroll.employer,
            "payslip_count": payroll.count,
        },
    });

    Ok(Json(json!({ "role": "admin", "personal": personal, "admin": admin })))
}
